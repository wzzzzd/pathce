//
//! Copyright 2020 Alibaba Group Holding Limited.
//!
//! Licensed under the Apache License, Version 2.0 (the "License");
//! you may not use this file except in compliance with the License.
//! You may obtain a copy of the License at
//!
//! http://www.apache.org/licenses/LICENSE-2.0
//!
//! Unless required by applicable law or agreed to in writing, software
//! distributed under the License is distributed on an "AS IS" BASIS,
//! WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//! See the License for the specific language governing permissions and
//! limitations under the License.

use core::ops::{Add, AddAssign};
use itertools::Itertools;
use ordered_float::{Float, OrderedFloat};
use std::borrow::BorrowMut;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::fmt::Display;
use std::sync::RwLock;

use ir_common::expr_parse::str_to_expr_pb;
use ir_common::generated::algebra::{self as pb};
use ir_common::generated::common::{self as common_pb, Variable};
use lazy_static::lazy_static;
use petgraph::graph::NodeIndex;

use crate::catalogue::catalog::{
    Approach, ApproachWeight, Catalogue, ExtendWeight, JoinWeight, PatMatPlanSpace,
};
use crate::catalogue::extend_step::DefiniteExtendStep;
use crate::catalogue::join_step::BinaryJoinPlan;
use crate::catalogue::pattern::{Pattern, PatternVertex};
use crate::catalogue::pattern_meta::PatternMeta;
use crate::catalogue::PatternDirection;
use crate::catalogue::{PatternId, PatternLabelId};
use crate::error::{IrError, IrResult};

lazy_static! {
    static ref ALPHA: RwLock<f64> = RwLock::new(0.15);
    static ref BETA: RwLock<f64> = RwLock::new(0.1);
    static ref W1: RwLock<f64> = RwLock::new(6.0);
    static ref W2: RwLock<f64> = RwLock::new(3.0);
}

/// Methods for Pattern to generate pb Logical plan of pattern matching
impl Pattern {
    /// Get all required subpatterns (i.e., whose cardinalities should be estimated) for plan generation.
    pub fn generate_subpatterns(&self) -> Vec<Pattern> {
        let mut patterns = BTreeMap::new();
        generate_subpatterns_recursive(self, &mut patterns);
        patterns.into_values().collect()
    }

    /// Generate a naive extend based pattern match plan
    pub fn generate_simple_extend_match_plan(
        &self, pattern_meta: &PatternMeta, is_distributed: bool,
    ) -> IrResult<pb::LogicalPlan> {
        let mut trace_pattern = self.clone();
        let mut definite_extend_steps = vec![];
        while trace_pattern.get_vertices_num() > 1 {
            let mut all_vertex_ids: Vec<PatternId> = trace_pattern
                .vertices_iter()
                .map(|v| v.get_id())
                .collect();
            sort_vertex_ids(&mut all_vertex_ids, &trace_pattern);
            let select_vertex_id = *all_vertex_ids.first().unwrap();
            let definite_extend_step =
                DefiniteExtendStep::from_target_pattern(&trace_pattern, select_vertex_id).unwrap();
            definite_extend_steps.push(definite_extend_step);
            trace_pattern = trace_pattern
                .remove_vertex(select_vertex_id)
                .unwrap();
        }
        definite_extend_steps.push(trace_pattern.try_into()?);
        let mut pb_plan = if is_distributed {
            build_distributed_match_plan(self, definite_extend_steps, pattern_meta)
                .expect("Failed to build distributed pattern match plan")
        } else {
            build_stand_alone_match_plan(self, definite_extend_steps, pattern_meta)
                .expect("Failed to build stand-alone pattern match plan")
        };
        match_pb_plan_add_source(&mut pb_plan);
        pb_plan_add_count_sink_operator(&mut pb_plan);
        Ok(pb_plan)
    }

    /// Generate heuristic plan that is not bad when patterns are not hit in catalog
    ///
    /// The basic idea is to put vertex with predicate or lowest cost to be executed earlier.
    pub fn generate_heuristic_match_plan(
        &self, catalog: &mut Catalogue, pattern_meta: &PatternMeta, is_distributed: bool,
    ) -> IrResult<pb::LogicalPlan> {
        // let (mut extend_steps, _) = get_definite_extend_steps(self.clone(), catalog);
        // extend_steps.reverse();
        // if is_distributed {
        //     build_distributed_match_plan(self, extend_steps, pattern_meta)
        // } else {
        //     build_stand_alone_match_plan(self, extend_steps, pattern_meta)
        // }

        let (mut extend_steps, _) = get_definite_extend_steps(self.clone(), catalog);
        extend_steps.reverse();
        let mut pb_plan = if is_distributed {
            build_distributed_match_plan(self, extend_steps, pattern_meta)
                .expect("Failed to build distributed pattern match plan")
        } else {
            build_stand_alone_match_plan(self, extend_steps, pattern_meta)
                .expect("Failed to build distributed pattern match plan")
        };
        match_pb_plan_add_source(&mut pb_plan);
        pb_plan_add_count_sink_operator(&mut pb_plan);
        Ok(pb_plan)
    }

    pub fn generate_optimized_match_plan(
        &self, catalog: &mut Catalogue, pattern_meta: &PatternMeta, is_distributed: bool,
    ) -> IrResult<pb::LogicalPlan> {
        // If pattern not found in catalogue, use heuristic plan
        if catalog
            .get_pattern_index(&self.encode_to())
            .is_none()
        {
            return self.generate_heuristic_match_plan(catalog, pattern_meta, is_distributed);
        }

        // If pattern is in catalogue, optimizatized plan is generated.
        match catalog.get_plan_space() {
            PatMatPlanSpace::BinaryJoin => Err(IrError::Unsupported(
                "Do not support pure binary join plan with no extend steps".to_string(),
            )),
            _ => {
                catalog.set_best_approach_by_pattern(self);
                PlanGenerator::new(self, catalog, pattern_meta, is_distributed)
                    .generate_pattern_match_plan()
            }
        }
    }
}

fn generate_subpatterns_recursive(pattern: &Pattern, patterns: &mut BTreeMap<Vec<u8>, Pattern>) {
    patterns
        .entry(pattern.encode_to())
        .or_insert_with(|| pattern.clone());

    let mut sub_patterns_extend_steps = vec![];
    for vertex_id in pattern
        .vertices_iter()
        .map(|vertex| vertex.get_id())
    {
        if let Some(sub_pattern) = pattern.clone().remove_vertex(vertex_id) {
            let extend_step = DefiniteExtendStep::from_target_pattern(pattern, vertex_id).unwrap();
            sub_patterns_extend_steps.push((sub_pattern, extend_step));
        }
    }
    for (sub_pattern, extend_step) in sub_patterns_extend_steps {
        patterns
            .entry(sub_pattern.encode_to())
            .or_insert_with(|| sub_pattern.clone());
        let target_vertex = extend_step.get_target_vertex();
        for extend_edge in extend_step.iter() {
            let adjacency_pattern = sub_pattern
                .extend_definitely(extend_edge, target_vertex)
                .unwrap();
            patterns
                .entry(adjacency_pattern.encode_to())
                .or_insert(adjacency_pattern);
        }
        for extend_edges in extend_step
            .iter()
            .permutations(extend_step.get_extend_edges_num())
        {
            let mut adjacency_pattern = sub_pattern.clone();
            for extend_edge in extend_edges {
                adjacency_pattern = adjacency_pattern
                    .extend_definitely(extend_edge, target_vertex)
                    .unwrap();
                patterns
                    .entry(adjacency_pattern.encode_to())
                    .or_insert_with(|| adjacency_pattern.clone());
            }
        }
        generate_subpatterns_recursive(&sub_pattern, patterns);
    }
}

impl Catalogue {
    pub fn set_best_approach_by_pattern(&mut self, pattern: &Pattern) {
        let node_index = self
            .get_pattern_index(&pattern.encode_to())
            .expect("Pattern not found in catalogue");
        self.set_node_best_approach_recursively(node_index)
            .expect("Failed to set node best approach recursively");
    }

    /// Given a node in catalogue, find the best approach and the lowest cost to reach to it
    fn set_node_best_approach_recursively(
        &mut self, node_index: NodeIndex,
    ) -> IrResult<(Option<Approach>, CostCount)> {
        let pattern_weight = self
            .get_pattern_weight(node_index)
            .expect("Failed to get pattern weight");
        let pattern = pattern_weight.get_pattern().clone();
        if pattern.get_vertices_num() == 1 {
            Ok((None, CostCount::from_src_pattern(pattern_weight.get_count())))
        } else if let Some(best_approach) = pattern_weight.get_best_approach() {
            // Recursively set best approach
            let pre_pattern_index = best_approach.get_src_pattern_index();
            let (_pre_best_approach, mut cost) = self
                .set_node_best_approach_recursively(pre_pattern_index)
                .expect("Failed to set node best approach recursively");
            let this_step_cost = self.estimate_approach_cost(&best_approach);
            cost += this_step_cost;
            Ok((Some(best_approach), cost))
        } else {
            let mut min_cost = CostCount::max_value();
            let candidate_approaches: Vec<Approach> = self.collect_candidate_approaches(node_index);
            if candidate_approaches.is_empty() {
                return Err(IrError::Unsupported("No approach found for pattern in catalog".to_string()));
            }

            let mut best_approach = candidate_approaches[0];
            let mut cost_counts_vec = vec![];
            for approach in candidate_approaches {
                let pre_pattern_index = approach.get_src_pattern_index();
                let (_pre_best_approach, pre_cost) = self
                    .set_node_best_approach_recursively(pre_pattern_index)
                    .expect("Failed to set node best approach recursively");
                let this_step_cost = self.estimate_approach_cost(&approach);
                let cost = pre_cost + this_step_cost;
                cost_counts_vec.push((pre_pattern_index, pre_cost, this_step_cost, cost));
                if cost < min_cost {
                    min_cost = cost;
                    best_approach = approach;
                }
            }
            print_pattern_choose_approach_log(
                self,
                &pattern,
                node_index,
                best_approach,
                min_cost,
                cost_counts_vec,
            );
            // set best approach in the catalogue
            self.set_pattern_best_approach(node_index, best_approach);
            Ok((Some(best_approach), min_cost))
        }
    }

    /// Collect all candidate approaches in plan space of the give node
    fn collect_candidate_approaches(&self, node_index: NodeIndex) -> Vec<Approach> {
        let candidate_approaches: Vec<Approach> = self
            .pattern_in_approaches_iter(node_index)
            .filter(|approach| {
                let approach_weight = self
                    .get_approach_weight(approach.get_approach_index())
                    .expect("No such approach exists in catalogue");
                let is_approach_in_plan_space: bool = match self.get_plan_space() {
                    PatMatPlanSpace::ExtendWithIntersection => approach_weight.is_extend(),
                    PatMatPlanSpace::BinaryJoin => approach_weight.is_join(),
                    PatMatPlanSpace::Hybrid => true,
                };
                is_approach_in_plan_space
            })
            .collect();
        candidate_approaches
    }

    /// Cost Estimation Functions
    fn estimate_approach_cost(&mut self, approach: &Approach) -> CostCount {
        let approach_weight = self
            .get_approach_weight(approach.get_approach_index())
            .expect("Approach not found in catalogue");
        if let ApproachWeight::ExtendStep(extend_weight) = approach_weight {
            self.estimate_extend_step_cost(approach, extend_weight)
        } else if let ApproachWeight::BinaryJoinStep(join_weight) = approach_weight.clone() {
            self.estimate_binary_join_step_cost(approach, &join_weight)
        } else {
            CostCount::max_value()
        }
    }

    /// Cost Estimation Function of Extend Step
    fn estimate_extend_step_cost(&self, approach: &Approach, extend_weight: &ExtendWeight) -> CostCount {
        let sub_pattern_count = self
            .get_pattern_weight(approach.get_src_pattern_index())
            .expect("Cannot find pattern weight in catalogue")
            .get_count();
        let pattern_count = self
            .get_pattern_weight(approach.get_target_pattern_index())
            .expect("Cannot find pattern weight in catalogue")
            .get_count();
        let adjacency_count = extend_weight.get_adjacency_count();
        let intersect_count = extend_weight.get_intersect_count();
        let extend_num = extend_weight
            .get_extend_step()
            .get_extend_edges_num();
        CostCount::from_extend(
            sub_pattern_count,
            pattern_count,
            adjacency_count,
            intersect_count,
            extend_num,
        )
    }

    /// Cost Estimation Function of Binary Join Step
    fn estimate_binary_join_step_cost(
        &mut self, approach: &Approach, join_weight: &JoinWeight,
    ) -> CostCount {
        // Collect data for cost estimation
        let build_pattern_cardinality = self
            .get_pattern_weight(approach.get_src_pattern_index())
            .expect("Cannot find pattern weight in catalogue")
            .get_count();
        let probe_pattern_cardinality = self
            .get_pattern_weight(join_weight.get_probe_pattern_node_index())
            .expect("Cannot find pattern weight in catalogue")
            .get_count();
        let joined_pattern_cardinality = self
            .get_pattern_weight(approach.get_target_pattern_index())
            .expect("Cannot find pattern weight in catalogue")
            .get_count();
        let (_, probe_pattern_cost) = self
            .set_node_best_approach_recursively(join_weight.get_probe_pattern_node_index())
            .unwrap();
        probe_pattern_cost
            + CostCount::from_join(
                build_pattern_cardinality,
                probe_pattern_cardinality,
                joined_pattern_cardinality,
            )
    }
}

/// ## Plan Generator for Catalogue
///
/// plan: pb logical plan
///
/// trace_pattern: the pattern traced for plan generator, and it will be changed recursively during generation.
///
/// target_pattern: the reference of the target pattern, fixed after initialization
///
/// catalog: the reference of the catalogue
pub struct PlanGenerator<'a> {
    plan: pb::LogicalPlan,
    vertex_labels_to_scan: BTreeSet<PatternLabelId>,
    trace_pattern: Pattern,
    target_pattern: &'a Pattern,
    catalog: &'a Catalogue,
    pattern_meta: &'a PatternMeta,
    is_distributed: bool,
}

impl<'a> PlanGenerator<'a> {
    pub fn new(
        pattern: &'a Pattern, catalog: &'a Catalogue, pattern_meta: &'a PatternMeta, is_distributed: bool,
    ) -> Self {
        PlanGenerator {
            catalog,
            is_distributed,
            pattern_meta,
            trace_pattern: pattern.clone(),
            target_pattern: pattern,
            plan: pb::LogicalPlan::default(),
            vertex_labels_to_scan: BTreeSet::new(),
        }
    }

    /// Get the pb logical plan
    pub fn get_pb_plan(&self) -> pb::LogicalPlan {
        self.plan.clone()
    }

    /// Return the number of nodes in the logical plan
    fn get_node_num(&self) -> usize {
        self.plan.nodes.len()
    }

    fn insert_node(&mut self, index: usize, node: pb::logical_plan::Node) -> IrResult<()> {
        if self.get_node_num() < index {
            return Err(IrError::Unsupported(
                "Failed in Plan Generation: Node Insertion, Index out of bound".to_string(),
            ));
        }

        self.plan.nodes.insert(index, node);
        // Offset the nodes after the inserted one by 1
        self.plan
            .nodes
            .iter_mut()
            .enumerate()
            .filter(|(current_node_idx, _node)| *current_node_idx > index)
            .for_each(|(_index, current_node)| {
                current_node
                    .children
                    .iter_mut()
                    .for_each(|child_id| {
                        *child_id += 1;
                    });

                // Offset the parent node id for each Intersect Node
                if let pb::logical_plan::operator::Opr::Intersect(intersect) = current_node
                    .opr
                    .as_mut()
                    .unwrap()
                    .opr
                    .as_mut()
                    .unwrap()
                {
                    intersect
                        .parents
                        .iter_mut()
                        .for_each(|parent_id| {
                            *parent_id += 1;
                        });
                }
            });
        Ok(())
    }

    fn remove_node(&mut self, index: usize) -> IrResult<()> {
        if self.get_node_num() < index {
            return Err(IrError::Unsupported(
                "Failed in Plan Generation: Node Removal, Index out of bound".to_string(),
            ));
        }

        // Offset the nodes after the inserted one by -1
        self.plan
            .nodes
            .iter_mut()
            .enumerate()
            .filter(|(current_node_idx, _node)| *current_node_idx > index)
            .for_each(|(_index, current_node)| {
                current_node
                    .children
                    .iter_mut()
                    .for_each(|child_id| {
                        *child_id -= 1;
                    });
            });
        // Remove the node at specified index
        self.plan.nodes.remove(index);

        Ok(())
    }

    pub fn generate_pattern_match_plan(&mut self) -> IrResult<pb::LogicalPlan> {
        self.generate_pattern_match_plan_recursively(self.target_pattern)
            .expect("Failed to generate pattern match plan with catalogue");
        self.match_pb_plan_add_source();
        self.pb_plan_add_count_sink_operator();
        Ok(self.plan.clone())
    }

    pub fn generate_pattern_match_plan_recursively(&mut self, pattern: &Pattern) -> IrResult<()> {
        // locate the pattern node in the catalog graph
        if let Some(node_index) = self
            .catalog
            .get_pattern_index(&pattern.encode_to())
        {
            if pattern.get_vertices_num() == 0 {
                return Err(IrError::InvalidPattern("No vertex in pattern".to_string()));
            } else if pattern.get_vertices_num() == 1 {
                self.generate_pattern_match_plan_for_size_one_pattern(pattern);
            } else {
                // Get the best approach to reach the node
                let best_approach_opt = self
                    .catalog
                    .get_pattern_weight(node_index)
                    .expect("Failed to get pattern weight from node index")
                    .get_best_approach();
                // Set trace pattern for recursive plan generation
                self.trace_pattern = pattern.clone();
                if let Some(best_approach) = best_approach_opt {
                    let approach_weight = self
                        .catalog
                        .get_approach_weight(best_approach.get_approach_index())
                        .expect("No sub approach exists in catalogue");
                    match approach_weight {
                        ApproachWeight::ExtendStep(_extend_weight) => {
                            self.generate_pattern_match_plan_recursively_for_extend_approach(best_approach);
                        }
                        ApproachWeight::BinaryJoinStep(_join_weight) => {
                            self.generate_pattern_match_plan_recursively_for_join_approach(best_approach);
                        }
                    }
                } else {
                    return Err(IrError::Unsupported(
                        "Best approach not found for pattern in catalog".to_string(),
                    ));
                }
            }
            Ok(())
        } else {
            Err(IrError::Unsupported("Pattern not found in catalog".to_string()))
        }
    }

    fn generate_pattern_match_plan_recursively_for_extend_approach(&mut self, extend_approach: Approach) {
        // Build logical plan for src pattern
        let target_pattern: Pattern = self.trace_pattern.clone();
        let pattern_index: NodeIndex = self
            .catalog
            .get_pattern_index(&target_pattern.encode_to())
            .expect("Pattern not found in catalog");
        let (src_pattern, definite_extend_step, _cost) =
            pattern_roll_back(self.trace_pattern.clone(), pattern_index, extend_approach, self.catalog);
        // Recursively generate pattern match plan for the source node
        self.generate_pattern_match_plan_recursively(&src_pattern)
            .expect("Failed to generate optimized pattern match plan recursively");
        // Append Extend Operator in logical plan
        self.append_extend_operator(&src_pattern, &target_pattern, definite_extend_step)
            .expect("Failed to append extend operator");
    }

    fn generate_pattern_match_plan_recursively_for_join_approach(&mut self, join_approach: Approach) {
        // Collect Join Weight
        let join_weight = self
            .catalog
            .get_approach_weight(join_approach.get_approach_index())
            .expect("No such approach exists in catalogue")
            .get_join_weight()
            .expect("Failed to get join weight");
        // Roll back join plan for exact pattern instances with vertex/edge id cohesion
        let join_plan = self
            .trace_pattern
            .binary_join_decomposition()
            .expect("Failed to do binary join decomposition")
            .into_iter()
            .filter(|binary_join_plan| {
                let build_pattern_in_catalog = join_weight.get_join_plan().get_build_pattern();
                pattern_equal(build_pattern_in_catalog, binary_join_plan.get_build_pattern())
                    || pattern_equal(build_pattern_in_catalog, binary_join_plan.get_probe_pattern())
            })
            .collect::<Vec<BinaryJoinPlan>>()
            .first()
            .expect("No valid join plan during pattern rollback")
            .clone();
        // Generate plan for build pattern
        self.generate_pattern_match_plan_recursively(join_plan.get_build_pattern())
            .expect("Failed to generate optimized pattern match plan recursively");
        // Generate plan for probe pattern
        let mut probe_pattern_logical_plan_builder = PlanGenerator::new(
            join_plan.get_probe_pattern(),
            self.catalog,
            self.pattern_meta,
            self.is_distributed,
        );
        probe_pattern_logical_plan_builder
            .generate_pattern_match_plan_recursively(join_plan.get_probe_pattern())
            .expect("Failed to generate optimized pattern match plan recursively");
        // Append binary join operator to join two plans
        let join_keys: Vec<Variable> = join_plan.generate_join_keys();
        self.join(probe_pattern_logical_plan_builder, join_keys)
            .expect("Failed to join two logical plans");
    }

    fn generate_pattern_match_plan_for_size_one_pattern(&mut self, pattern: &Pattern) {
        // Source operator when there is only one vertex in pattern
        let vertex: &PatternVertex = pattern
            .vertices_iter()
            .collect::<Vec<&PatternVertex>>()[0];
        let vertex_label: PatternLabelId = vertex.get_label();
        // Append select node
        let select_node = {
            let opr = pb::Select {
                predicate: Some(str_to_expr_pb(format!("@.~label == {}", vertex_label)).unwrap()),
            };
            let children: Vec<i32> = vec![1];
            pb::logical_plan::Node { opr: Some(opr.into()), children }
        };
        self.plan.nodes.push(select_node);
        // Append as node
        let as_node = {
            let opr = pb::As { alias: Some((vertex.get_id() as i32).into()) };
            let children: Vec<i32> = vec![2];
            pb::logical_plan::Node { opr: Some(opr.into()), children }
        };
        self.plan.nodes.push(as_node);
        // Insert the vertex label to scan
        self.vertex_labels_to_scan
            .insert(vertex.get_label());
        // Set root for pb plan
        self.plan.roots = vec![0];
    }

    /// Append logical plan operators for extend step
    /// ```text
    ///             source
    ///           /   |    \
    ///        ...Edge Expand...
    ///           \   |    /
    ///            Intersect
    /// ```
    fn append_extend_operator(
        &mut self, src_pattern: &Pattern, target_pattern: &Pattern,
        definite_extend_step: DefiniteExtendStep,
    ) -> IrResult<()> {
        if self.is_distributed {
            self.append_extend_operator_distributed(src_pattern, target_pattern, definite_extend_step)
                .expect("Failed to append extend operator in distributed mode");
        } else {
            self.append_extend_operator_stand_alone(src_pattern, target_pattern, definite_extend_step)
                .expect("Failed to append extend operator in stand-alone mode");
        }

        Ok(())
    }

    fn append_extend_operator_distributed(
        &mut self, src_pattern: &Pattern, target_pattern: &Pattern,
        definite_extend_step: DefiniteExtendStep,
    ) -> IrResult<()> {
        // Modify Children ID in the last node of the input plan
        let mut child_offset = self.get_node_num() as i32;
        let edge_expands: Vec<pb::EdgeExpand> =
            definite_extend_step.generate_expand_operators(target_pattern);
        let edge_expands_num = edge_expands.len();
        let edge_expands_ids: Vec<i32> = (0..edge_expands_num as i32)
            .map(|i| i + child_offset)
            .collect();
        // if edge expand num > 1, we need a Intersect Operator
        match edge_expands_num.cmp(&1) {
            Ordering::Greater => {
                // Set the children of the previous node
                self.plan.nodes.last_mut().unwrap().children = edge_expands_ids.clone();
                // Append edge expand nodes
                child_offset += edge_expands.len() as i32;
                for edge_expand in edge_expands {
                    let edge_expand_node = pb::logical_plan::Node {
                        opr: Some(edge_expand.into()),
                        children: vec![child_offset],
                    };
                    self.plan.nodes.push(edge_expand_node);
                }
                child_offset += 1;
                // Append Intersect node
                let intersect_node = {
                    let opr = definite_extend_step.generate_intersect_operator(edge_expands_ids);
                    let children: Vec<i32> = vec![child_offset];
                    pb::logical_plan::Node { opr: Some(opr.into()), children }
                };
                self.plan.nodes.push(intersect_node);
                child_offset += 1;
            }
            Ordering::Equal => {
                // Set the children of the previous node
                self.plan.nodes.last_mut().unwrap().children = edge_expands_ids;
                // Append edge expand node
                child_offset += edge_expands.len() as i32;
                let edge_expand_node = {
                    let opr = edge_expands
                        .into_iter()
                        .last()
                        .expect("Failed to get edge expand operator");
                    let children: Vec<i32> = vec![child_offset];
                    pb::logical_plan::Node { opr: Some(opr.into()), children }
                };
                self.plan.nodes.push(edge_expand_node);
                child_offset += 1;
            }
            _ => {
                return Err(IrError::InvalidPattern(
                    "Build logical plan error: extend step is not source but has 0 edges".to_string(),
                ));
            }
        }
        // Filter by the label of target vertex
        if check_target_vertex_label_num(&definite_extend_step, self.pattern_meta) > 1 {
            let select_node = {
                let target_vertex_label = definite_extend_step
                    .get_target_vertex()
                    .get_label();
                let opr = pb::Select {
                    predicate: Some(
                        str_to_expr_pb(format!("@.~label == {}", target_vertex_label)).unwrap(),
                    ),
                };
                let children: Vec<i32> = vec![child_offset];
                pb::logical_plan::Node { opr: Some(opr.into()), children }
            };
            self.plan.nodes.push(select_node);
            child_offset += 1;
        }
        // Filter by the predicate of target vertex
        if let Some(filter) = definite_extend_step.generate_vertex_filter_operator(src_pattern) {
            let select_node =
                pb::logical_plan::Node { opr: Some(filter.into()), children: vec![child_offset] };
            self.plan.nodes.push(select_node);
        }

        Ok(())
    }

    fn append_extend_operator_stand_alone(
        &mut self, src_pattern: &Pattern, target_pattern: &Pattern,
        definite_extend_step: DefiniteExtendStep,
    ) -> IrResult<()> {
        // Check if current node is the parent node's child
        let current_offset = self.get_node_num() as i32;
        let parent_offset = self.get_node_num() - 1;
        let parent_node = self.plan.nodes.get_mut(parent_offset).unwrap();
        if !parent_node.children.contains(&current_offset) {
            parent_node.children.push(current_offset);
        }

        // Modify Children ID in the last node of the input plan
        let mut child_offset = (self.get_node_num() + 1) as i32;
        let mut edge_expands: Vec<pb::EdgeExpand> =
            definite_extend_step.generate_expand_operators(target_pattern);
        let edge_expands_num = edge_expands.len();
        // Append Edge Expand Nodes
        if edge_expands_num == 0 {
            return Err(IrError::InvalidPattern(
                "Build logical plan error: extend step is not source but has 0 edges".to_string(),
            ));
        } else if edge_expands_num == 1 {
            let edge_expand_node = {
                let opr = edge_expands.remove(0);
                let children: Vec<i32> = vec![child_offset];
                pb::logical_plan::Node { opr: Some(opr.into()), children }
            };
            self.plan.nodes.push(edge_expand_node);
            child_offset += 1;
        } else {
            let expand_intersect_node = {
                let opr = pb::ExpandAndIntersect { edge_expands };
                let children: Vec<i32> = vec![child_offset];
                pb::logical_plan::Node { opr: Some(opr.into()), children }
            };
            self.plan.nodes.push(expand_intersect_node);
            child_offset += 1;
        }
        // Filter on the label of target vertex
        if check_target_vertex_label_num(&definite_extend_step, self.pattern_meta) > 1 {
            let select_ndoe = {
                let target_vertex_label = definite_extend_step
                    .get_target_vertex()
                    .get_label();
                let opr = pb::Select {
                    predicate: Some(
                        str_to_expr_pb(format!("@.~label == {}", target_vertex_label)).unwrap(),
                    ),
                };
                let children: Vec<i32> = vec![child_offset];
                pb::logical_plan::Node { opr: Some(opr.into()), children }
            };
            self.plan.nodes.push(select_ndoe);
            child_offset += 1;
        }
        // Filter on the predicate of target vertex
        if let Some(filter) = definite_extend_step.generate_vertex_filter_operator(src_pattern) {
            let select_node = {
                let opr = filter;
                let children: Vec<i32> = vec![child_offset];
                pb::logical_plan::Node { opr: Some(opr.into()), children }
            };
            self.plan.nodes.push(select_node);
        }

        Ok(())
    }

    /// Join two logical plan builder, resulting in one logical plan builder with join operator
    pub fn join(&mut self, mut other: PlanGenerator, join_keys: Vec<Variable>) -> IrResult<()> {
        // Add an as node with alias = None for binary join
        let as_node_for_join = {
            let opr = pb::As { alias: None };
            let children: Vec<i32> = vec![self.plan.roots[0] + 1, self.get_node_num() as i32 + 1];
            pb::logical_plan::Node { opr: Some(opr.into()), children }
        };
        self.insert_node(0, as_node_for_join)
            .expect("Failed to insert node to pb_plan");

        // Offset children IDs for the other logical plan
        let left_size = self.get_node_num();
        let right_size = other.get_node_num();
        other.plan.nodes.iter_mut().for_each(|node| {
            node.children.iter_mut().for_each(|child| {
                *child += left_size as i32;
            });
            // Offset the parent node id for each Intersect Node
            if let pb::logical_plan::operator::Opr::Intersect(intersect) =
                node.opr.as_mut().unwrap().opr.as_mut().unwrap()
            {
                intersect
                    .parents
                    .iter_mut()
                    .for_each(|parent_id| {
                        *parent_id += left_size as i32;
                    });
            }
        });

        // Link the last node of both left and right plan to the join node
        let join_node_idx = left_size + right_size;
        if let Some(node) = self.plan.nodes.last_mut() {
            // node.children.push(join_node_idx as i32);
            node.children = vec![join_node_idx as i32];
        } else {
            return Err(IrError::Unsupported("No node in binary join component".to_string()));
        }

        if let Some(node) = other.plan.nodes.last_mut() {
            // node.children.push(join_node_idx as i32);
            node.children = vec![join_node_idx as i32];
        } else {
            return Err(IrError::Unsupported("No node in binary join component".to_string()));
        }

        // Concat two plans to be one
        self.plan.nodes.extend(other.plan.nodes);
        // Append join node
        let join_node = {
            let opr = pb::Join {
                left_keys: join_keys.clone(),
                right_keys: join_keys,
                kind: pb::join::JoinKind::Inner as i32,
            };
            let children: Vec<i32> = vec![];
            pb::logical_plan::Node { opr: Some(opr.into()), children }
        };
        self.plan.nodes.push(join_node);
        // Merge vertex labels to scan
        self.vertex_labels_to_scan
            .append(&mut other.vertex_labels_to_scan.clone());

        Ok(())
    }

    pub fn match_pb_plan_add_source(&mut self) {
        // // Iterate through all nodes and collect Select nodes
        // let mut vertex_labels_to_scan: Vec<PatternLabelId> = vec![];
        // self.plan.nodes.iter().for_each(|node| {
        //     if let pb::logical_plan::operator::Opr::Select(first_select) = node
        //         .opr
        //         .as_ref()
        //         .unwrap()
        //         .opr
        //         .as_ref()
        //         .unwrap()
        //         .clone()
        //     {
        //         let label_id: PatternLabelId = first_select
        //             .predicate
        //             .as_ref()
        //             .unwrap()
        //             .operators
        //             .get(2)
        //             .and_then(|opr| opr.item.as_ref())
        //             .and_then(|item| {
        //                 if let common_pb::expr_opr::Item::Const(value) = item {
        //                     Some(value)
        //                 } else {
        //                     None
        //                 }
        //             })
        //             .and_then(|value| {
        //                 if let Some(common_pb::value::Item::I64(label_id)) = value.item {
        //                     Some(label_id as i32)
        //                 } else if let Some(common_pb::value::Item::I32(label_id)) = value.item {
        //                     Some(label_id)
        //                 } else {
        //                     None
        //                 }
        //             })
        //             .expect("Failed to get vertex label from Select Node");
        //         vertex_labels_to_scan.push(label_id);
        //     }
        // });

        // If the plan is purely extend-based, the first Select node could be removed, and we only need to scan the first vertex
        if let PatMatPlanSpace::ExtendWithIntersection = self.catalog.get_plan_space() {
            self.remove_node(0)
                .expect("Failed to remove node from pb_plan");
        }

        // Append Sink Node
        let scan_node = {
            let opr = pb::Scan {
                scan_opt: 0,
                alias: None,
                params: Some(pb::QueryParams {
                    tables: self
                        .vertex_labels_to_scan
                        .clone()
                        .into_iter()
                        .map(|v_label| v_label.into())
                        .collect(),
                    columns: vec![],
                    is_all_columns: false,
                    limit: None,
                    predicate: None,
                    sample_ratio: 1.0,
                    extra: HashMap::new(),
                }),
                idx_predicate: None,
            };
            let children: Vec<i32> = vec![1];
            pb::logical_plan::Node { opr: Some(opr.into()), children }
        };
        self.insert_node(0, scan_node)
            .expect("Failed to insert node to pb_plan");
    }

    pub fn pb_plan_add_count_sink_operator(&mut self) {
        let pb_plan_len = self.plan.nodes.len();
        // Modify the children ID of the last node
        self.plan.nodes[pb_plan_len - 1].children = vec![pb_plan_len as i32];
        // Append Count Aggregate Node
        let count_node = {
            let opr = pb::GroupBy {
                mappings: vec![],
                functions: vec![pb::group_by::AggFunc {
                    vars: vec![],
                    aggregate: 3, // count
                    alias: Some(0.into()),
                }],
            };
            let children: Vec<i32> = vec![(pb_plan_len + 1) as i32];
            pb::logical_plan::Node { opr: Some(opr.into()), children }
        };
        self.plan.nodes.push(count_node);
        // Append Sink Node
        let sink_node = {
            let opr = pb::Sink {
                tags: vec![common_pb::NameOrIdKey { key: Some(0.into()) }],
                sink_target: Some(pb::sink::SinkTarget {
                    inner: Some(pb::sink::sink_target::Inner::SinkDefault(pb::SinkDefault {
                        id_name_mappings: vec![],
                    })),
                }),
            };
            let children: Vec<i32> = vec![];
            pb::logical_plan::Node { opr: Some(opr.into()), children }
        };
        self.plan.nodes.push(sink_node);
    }
}

pub fn get_definite_extend_steps(
    pattern: Pattern, catalog: &mut Catalogue,
) -> (Vec<DefiniteExtendStep>, CostCount) {
    let pattern_code = pattern.encode_to();
    if let Some(pattern_index) = catalog.get_pattern_index(&pattern_code) {
        get_definite_extend_steps_in_catalog(catalog, pattern_index, pattern)
    } else {
        let pattern_count = catalog.estimate_pattern_count(&pattern);
        let mut sub_patterns_extend_steps = vec![];
        for vertex_id in pattern
            .vertices_iter()
            .map(|vertex| vertex.get_id())
        {
            if let Some(sub_pattern) = pattern.clone().remove_vertex(vertex_id) {
                let extend_step = DefiniteExtendStep::from_target_pattern(&pattern, vertex_id).unwrap();
                sub_patterns_extend_steps.push((sub_pattern, extend_step));
            }
        }
        let mut optimal_extend_steps = vec![];
        let mut min_cost = CostCount::max_value();
        let mut max_predicate_num = usize::MIN;
        for (sub_pattern, mut extend_step) in sub_patterns_extend_steps {
            let sub_pattern_predicate_num = sub_pattern.get_predicate_num();
            let sub_pattern_count = catalog.estimate_pattern_count(&sub_pattern);
            let adjacency_count = get_adjacency_count(&sub_pattern, &mut extend_step, catalog);
            let intersect_count = get_intersect_count(&sub_pattern, &extend_step, catalog);
            let (mut extend_steps, pre_cost) = get_definite_extend_steps(sub_pattern, catalog);
            let this_step_cost = CostCount::from_extend(
                sub_pattern_count,
                pattern_count,
                adjacency_count,
                intersect_count,
                extend_step.get_extend_edges_num(),
            );

            if sub_pattern_predicate_num > max_predicate_num
                || (pre_cost + this_step_cost < min_cost && sub_pattern_predicate_num == max_predicate_num)
            {
                extend_steps.push(extend_step);
                optimal_extend_steps = extend_steps;
                min_cost = pre_cost + this_step_cost;
                max_predicate_num = sub_pattern_predicate_num;
            }
        }
        (optimal_extend_steps, min_cost)
    }
}

fn get_adjacency_count(
    sub_pattern: &Pattern, extend_step: &mut DefiniteExtendStep, catalog: &Catalogue,
) -> OrderedFloat<f64> {
    let target_vertex = extend_step.get_target_vertex();
    let mut adjacency_count_map = HashMap::new();
    for extend_edge in extend_step.iter() {
        let adjacency_pattern = sub_pattern
            .extend_definitely(extend_edge, target_vertex)
            .unwrap();
        let sub_target_pattern_count = catalog.estimate_pattern_count(&adjacency_pattern);
        adjacency_count_map.insert(extend_edge.get_edge_id(), sub_target_pattern_count);
    }
    extend_step.sort_by(|extend_edge1, extend_edge2| {
        adjacency_count_map
            .get(&extend_edge1.get_edge_id())
            .cmp(&adjacency_count_map.get(&extend_edge2.get_edge_id()))
    });
    adjacency_count_map.values().sum()
}

fn get_intersect_count(
    sub_pattern: &Pattern, extend_step: &DefiniteExtendStep, catalog: &Catalogue,
) -> OrderedFloat<f64> {
    let target_vertex = extend_step.get_target_vertex();
    let mut intersect_count = OrderedFloat::default();
    let mut adjacency_pattern = sub_pattern.clone();
    for (i, extend_edge) in extend_step.iter().enumerate() {
        if i + 1 == extend_step.get_extend_edges_num() {
            break;
        }
        adjacency_pattern = adjacency_pattern
            .extend_definitely(extend_edge, target_vertex)
            .unwrap();
        let sub_target_pattern_count = catalog.estimate_pattern_count(&adjacency_pattern);
        intersect_count += sub_target_pattern_count
    }
    intersect_count
}

fn get_definite_extend_steps_in_catalog(
    catalog: &mut Catalogue, pattern_index: NodeIndex, pattern: Pattern,
) -> (Vec<DefiniteExtendStep>, CostCount) {
    let pattern_weight = catalog
        .get_pattern_weight(pattern_index)
        .unwrap();
    let predicate_num = pattern.get_predicate_num();
    if pattern.get_vertices_num() == 1 {
        let src_definite_extend_step = DefiniteExtendStep::try_from(pattern).unwrap();
        let cost = CostCount::from_src_pattern(pattern_weight.get_count());
        (vec![src_definite_extend_step], cost)
    } else if pattern_weight.get_best_approach().is_some() && predicate_num == 0 {
        let best_approach = pattern_weight.get_best_approach().unwrap();
        let (pre_pattern, definite_extend_step, this_step_cost) =
            pattern_roll_back(pattern, pattern_index, best_approach, catalog);
        let pre_pattern_index = best_approach.get_src_pattern_index();
        let (mut definite_extend_steps, mut cost) =
            get_definite_extend_steps_in_catalog(catalog, pre_pattern_index, pre_pattern);
        definite_extend_steps.push(definite_extend_step);
        cost += this_step_cost;
        return (definite_extend_steps, cost);
    } else {
        let mut optimal_extend_steps = vec![];
        let mut min_cost = CostCount::max_value();
        let mut max_predicate_num = usize::MIN;
        let approaches: Vec<Approach> = catalog
            .pattern_in_approaches_iter(pattern_index)
            .collect();
        let mut best_approach = approaches[0];
        let mut cost_counts_vec = vec![];
        for approach in approaches {
            let (pre_pattern, definite_extend_step, this_step_cost) =
                pattern_roll_back(pattern.clone(), pattern_index, approach, catalog);
            let pre_pattern_predicate_num = pre_pattern.get_predicate_num();
            let pre_pattern_index = approach.get_src_pattern_index();
            let (mut extend_steps, pre_cost) =
                get_definite_extend_steps_in_catalog(catalog, pre_pattern_index, pre_pattern);
            extend_steps.push(definite_extend_step);
            let cost = pre_cost + this_step_cost;
            cost_counts_vec.push((pre_pattern_index, pre_cost, this_step_cost, cost));
            if pre_pattern_predicate_num > max_predicate_num
                || (cost < min_cost && pre_pattern_predicate_num == max_predicate_num)
            {
                optimal_extend_steps = extend_steps;
                min_cost = cost;
                max_predicate_num = pre_pattern_predicate_num;
                best_approach = approach;
            }
        }
        print_pattern_choose_approach_log(
            catalog,
            &pattern,
            pattern_index,
            best_approach,
            min_cost,
            cost_counts_vec,
        );
        if predicate_num == 0 {
            catalog.set_pattern_best_approach(pattern_index, best_approach);
        }
        return (optimal_extend_steps, min_cost);
    }
}

fn pattern_roll_back(
    pattern: Pattern, pattern_index: NodeIndex, approach: Approach, catalog: &Catalogue,
) -> (Pattern, DefiniteExtendStep, CostCount) {
    let pattern_weight = catalog
        .get_pattern_weight(pattern_index)
        .unwrap();
    let extend_weight = catalog
        .get_extend_weight(approach.get_approach_index())
        .unwrap();
    let pre_pattern_index = approach.get_src_pattern_index();
    let pre_pattern_weight = catalog
        .get_pattern_weight(pre_pattern_index)
        .unwrap();
    let this_step_cost = CostCount::from_extend(
        pre_pattern_weight.get_count(),
        pattern_weight.get_count(),
        extend_weight.get_adjacency_count(),
        extend_weight.get_intersect_count(),
        extend_weight
            .get_extend_step()
            .get_extend_edges_num(),
    );
    let target_vertex_id = pattern
        .get_vertex_from_rank(extend_weight.get_target_vertex_rank())
        .unwrap()
        .get_id();
    let extend_step = extend_weight.get_extend_step();
    let edge_id_map = pattern
        .adjacencies_iter(target_vertex_id)
        .map(|adjacency| {
            (
                (
                    adjacency.get_adj_vertex().get_id(),
                    adjacency.get_edge_label(),
                    adjacency.get_direction().reverse(),
                ),
                adjacency.get_edge_id(),
            )
        })
        .collect();
    let pre_pattern = pattern
        .remove_vertex(target_vertex_id)
        .expect("Failed to remove vertex from pattern");
    let definite_extend_step =
        DefiniteExtendStep::from_src_pattern(&pre_pattern, extend_step, target_vertex_id, edge_id_map)
            .expect("Failed to build DefiniteExtendStep from src pattern");
    (pre_pattern, definite_extend_step, this_step_cost)
}

fn vertex_has_predicate(pattern: &Pattern, vertex_id: PatternId) -> bool {
    pattern
        .get_vertex_predicate(vertex_id)
        .is_some()
}

fn get_adj_edges_filter_num(pattern: &Pattern, vertex_id: PatternId) -> usize {
    pattern
        .adjacencies_iter(vertex_id)
        .filter(|adj| {
            pattern
                .get_edge_predicate(adj.get_edge_id())
                .is_some()
        })
        .count()
}

fn sort_vertex_ids(vertex_ids: &mut [PatternId], pattern: &Pattern) {
    vertex_ids.sort_by(|&v1_id, &v2_id| {
        // compare v1 and v2's vertex predicate
        let v1_has_predicate = vertex_has_predicate(pattern, v1_id);
        let v2_has_predicate = vertex_has_predicate(pattern, v2_id);
        // compare v1 and v2's adjacent edges' predicate num
        let v1_edges_predicate_num = get_adj_edges_filter_num(pattern, v1_id);
        let v2_edges_predicate_num = get_adj_edges_filter_num(pattern, v2_id);
        // compare v1 and v2's degree
        let v1_degree = pattern.get_vertex_degree(v1_id);
        let v2_degree = pattern.get_vertex_degree(v2_id);
        // compare v1 and v2's out degree
        let v1_out_degree = pattern.get_vertex_out_degree(v1_id);
        let v2_out_degree = pattern.get_vertex_out_degree(v2_id);
        (v1_has_predicate, v1_edges_predicate_num, v1_degree, v1_out_degree).cmp(&(
            v2_has_predicate,
            v2_edges_predicate_num,
            v2_degree,
            v2_out_degree,
        ))
    });
}

/// Build logical plan for extend based pattern match plan
///             source
///           /   |    \
///           \   |    /
///            intersect
pub fn build_distributed_match_plan(
    origin_pattern: &Pattern, mut definite_extend_steps: Vec<DefiniteExtendStep>,
    pattern_meta: &PatternMeta,
) -> IrResult<pb::LogicalPlan> {
    let mut match_plan = pb::LogicalPlan::default();
    let mut child_offset = 1;
    let as_opr = generate_add_start_nodes(
        &mut match_plan,
        origin_pattern,
        &mut definite_extend_steps,
        &mut child_offset,
    )?;
    // pre_node will have some children, need a subplan
    let mut pre_node = pb::logical_plan::Node { opr: Some(as_opr.into()), children: vec![] };
    for definite_extend_step in definite_extend_steps.into_iter().rev() {
        let edge_expands = definite_extend_step.generate_expand_operators(origin_pattern);
        let edge_expands_num = edge_expands.len();
        let edge_expands_ids: Vec<i32> = (0..edge_expands_num as i32)
            .map(|i| i + child_offset)
            .collect();
        for &i in edge_expands_ids.iter() {
            pre_node.children.push(i);
        }
        match_plan.nodes.push(pre_node);
        match edge_expands_num.cmp(&1) {
            Ordering::Greater => {
                // if edge expand num > 1, we need a Intersect Operator
                for edge_expand in edge_expands {
                    let node = pb::logical_plan::Node {
                        opr: Some(edge_expand.into()),
                        children: vec![child_offset + edge_expands_num as i32],
                    };
                    match_plan.nodes.push(node);
                }
                let intersect = definite_extend_step.generate_intersect_operator(edge_expands_ids);
                pre_node = pb::logical_plan::Node { opr: Some(intersect.into()), children: vec![] };
                child_offset += (edge_expands_num + 1) as i32;
            }
            Ordering::Equal => {
                let edge_expand = edge_expands.into_iter().last().unwrap();
                pre_node = pb::logical_plan::Node { opr: Some(edge_expand.into()), children: vec![] };
                child_offset += 1;
            }
            _ => {
                return Err(IrError::InvalidPattern(
                    "Build logical plan error: extend step is not source but has 0 edges".to_string(),
                ));
            }
        }
        if check_target_vertex_label_num(&definite_extend_step, pattern_meta) > 1 {
            let target_vertex_label = definite_extend_step
                .get_target_vertex()
                .get_label();
            let label_filter = pb::Select {
                predicate: Some(str_to_expr_pb(format!("@.~label == {}", target_vertex_label)).unwrap()),
            };
            let label_filter_id = child_offset;
            pre_node.children.push(label_filter_id);
            match_plan.nodes.push(pre_node);
            pre_node = pb::logical_plan::Node { opr: Some(label_filter.into()), children: vec![] };
            child_offset += 1;
        }
        if let Some(filter) = definite_extend_step.generate_vertex_filter_operator(origin_pattern) {
            let filter_id = child_offset;
            pre_node.children.push(filter_id);
            match_plan.nodes.push(pre_node);
            pre_node = pb::logical_plan::Node { opr: Some(filter.into()), children: vec![] };
            child_offset += 1;
        }
    }
    pre_node.children.push(child_offset);
    match_plan.nodes.push(pre_node);
    Ok(match_plan)
}

pub fn build_stand_alone_match_plan(
    origin_pattern: &Pattern, mut definite_extend_steps: Vec<DefiniteExtendStep>,
    pattern_meta: &PatternMeta,
) -> IrResult<pb::LogicalPlan> {
    let mut match_plan = pb::LogicalPlan::default();
    let mut child_offset = 1;
    let as_opr = generate_add_start_nodes(
        &mut match_plan,
        origin_pattern,
        &mut definite_extend_steps,
        &mut child_offset,
    )?;
    match_plan
        .nodes
        .push(pb::logical_plan::Node { opr: Some(as_opr.into()), children: vec![child_offset] });
    child_offset += 1;
    for definite_extend_step in definite_extend_steps.into_iter().rev() {
        let mut edge_expands = definite_extend_step.generate_expand_operators(origin_pattern);
        if edge_expands.is_empty() {
            return Err(IrError::InvalidPattern(
                "Build logical plan error: extend step is not source but has 0 edges".to_string(),
            ));
        } else if edge_expands.len() == 1 {
            match_plan.nodes.push(pb::logical_plan::Node {
                opr: Some(edge_expands.remove(0).into()),
                children: vec![child_offset],
            });
            child_offset += 1;
        } else {
            let expand_intersect_opr = pb::ExpandAndIntersect { edge_expands };
            match_plan.nodes.push(pb::logical_plan::Node {
                opr: Some(expand_intersect_opr.into()),
                children: vec![child_offset],
            });
            child_offset += 1;
        }

        if check_target_vertex_label_num(&definite_extend_step, pattern_meta) > 1 {
            let target_vertex_label = definite_extend_step
                .get_target_vertex()
                .get_label();
            let label_filter = pb::Select {
                predicate: Some(str_to_expr_pb(format!("@.~label == {}", target_vertex_label)).unwrap()),
            };
            match_plan.nodes.push(pb::logical_plan::Node {
                opr: Some(label_filter.into()),
                children: vec![child_offset],
            });
            child_offset += 1;
        }
        if let Some(filter) = definite_extend_step.generate_vertex_filter_operator(origin_pattern) {
            match_plan
                .nodes
                .push(pb::logical_plan::Node { opr: Some(filter.into()), children: vec![child_offset] });
            child_offset += 1;
        }
    }
    Ok(match_plan)
}

/// Cost estimation functions
pub fn extend_cost_estimate(
    pre_pattern_count: usize, pattern_count: usize, adjacency_count: usize, intersect_count: usize,
    extend_num: usize,
) -> usize {
    pre_pattern_count
        + pattern_count
        + (if extend_num > 1 {
            ((adjacency_count as f64) * (*ALPHA.read().unwrap())) as usize
                + ((intersect_count as f64) * (*BETA.read().unwrap())) as usize
                + pre_pattern_count * extend_num
        } else {
            0
        })
}

fn generate_add_start_nodes(
    match_plan: &mut pb::LogicalPlan, origin_pattern: &Pattern,
    definite_extend_steps: &mut Vec<DefiniteExtendStep>, child_offset: &mut i32,
) -> IrResult<pb::As> {
    let source_extend = definite_extend_steps
        .pop()
        .ok_or(IrError::InvalidPattern("Build logical plan error: from empty extend steps!".to_string()))?;
    let source_vertex = source_extend.get_target_vertex();
    let source_vertex_label = source_vertex.get_label();
    let source_vertex_id = source_vertex.get_id();
    let label_select = pb::Select {
        predicate: Some(str_to_expr_pb(format!("@.~label == {}", source_vertex_label)).unwrap()),
    };
    match_plan
        .nodes
        .push(pb::logical_plan::Node { opr: Some(label_select.into()), children: vec![*child_offset] });
    *child_offset += 1;
    if let Some(filter) = source_extend.generate_vertex_filter_operator(origin_pattern) {
        match_plan
            .nodes
            .push(pb::logical_plan::Node { opr: Some(filter.into()), children: vec![*child_offset] });
        *child_offset += 1;
    }
    let as_opr = pb::As { alias: Some((source_vertex_id as i32).into()) };
    Ok(as_opr)
}

fn check_target_vertex_label_num(extend_step: &DefiniteExtendStep, pattern_meta: &PatternMeta) -> usize {
    let mut target_vertex_labels = HashSet::new();
    for (i, extend_edge) in extend_step.iter().enumerate() {
        let mut target_vertex_label_candis = HashSet::new();
        let src_vertex_label = extend_edge.get_src_vertex().get_label();
        let edge_label = extend_edge.get_edge_label();
        let dir = extend_edge.get_direction();
        for (start_vertex_label, end_vertex_label) in
            pattern_meta.associated_vlabels_iter_by_elabel(edge_label)
        {
            if dir == PatternDirection::Out && src_vertex_label == start_vertex_label {
                target_vertex_label_candis.insert(end_vertex_label);
            } else if dir == PatternDirection::In && src_vertex_label == end_vertex_label {
                target_vertex_label_candis.insert(start_vertex_label);
            }
        }
        if i == 0 {
            target_vertex_labels = target_vertex_label_candis;
        } else {
            target_vertex_labels = target_vertex_labels
                .intersection(&target_vertex_label_candis)
                .cloned()
                .collect();
        }
    }
    target_vertex_labels.len()
}

fn match_pb_plan_add_source(pb_plan: &mut pb::LogicalPlan) -> Option<()> {
    if let pb::logical_plan::operator::Opr::Select(first_select) = pb_plan
        .nodes
        .first()
        .unwrap()
        .opr
        .as_ref()
        .unwrap()
        .opr
        .as_ref()
        .unwrap()
        .clone()
    {
        let label_id = first_select
            .predicate
            .as_ref()
            .unwrap()
            .operators
            .get(2)
            .and_then(|opr| opr.item.as_ref())
            .and_then(
                |item| if let common_pb::expr_opr::Item::Const(value) = item { Some(value) } else { None },
            )
            .and_then(|value| {
                if let Some(common_pb::value::Item::I64(label_id)) = value.item {
                    Some(label_id as i32)
                } else if let Some(common_pb::value::Item::I32(label_id)) = value.item {
                    Some(label_id)
                } else {
                    None
                }
            })
            .unwrap();
        let source = pb::Scan {
            scan_opt: 0,
            alias: None,
            params: Some(pb::QueryParams {
                tables: vec![label_id.into()],
                columns: vec![],
                is_all_columns: false,
                limit: None,
                predicate: None,
                sample_ratio: 1.0,
                extra: HashMap::new(),
            }),
            idx_predicate: None,
        };
        pb_plan.nodes.remove(0);
        pb_plan
            .nodes
            .insert(0, pb::logical_plan::Node { opr: Some(source.into()), children: vec![1] });
        Some(())
    } else {
        None
    }
}

fn pb_plan_add_count_sink_operator(pb_plan: &mut pb::LogicalPlan) {
    let pb_plan_len = pb_plan.nodes.len();
    let count = pb::GroupBy {
        mappings: vec![],
        functions: vec![pb::group_by::AggFunc {
            vars: vec![],
            aggregate: 3, // count
            alias: Some(0.into()),
        }],
    };
    pb_plan
        .nodes
        .push(pb::logical_plan::Node { opr: Some(count.into()), children: vec![(pb_plan_len + 1) as i32] });
    let sink = pb::Sink {
        tags: vec![common_pb::NameOrIdKey { key: Some(0.into()) }],
        sink_target: Some(pb::sink::SinkTarget {
            inner: Some(pb::sink::sink_target::Inner::SinkDefault(pb::SinkDefault {
                id_name_mappings: vec![],
            })),
        }),
    };
    pb_plan
        .nodes
        .push(pb::logical_plan::Node { opr: Some(sink.into()), children: vec![] });
}

fn pattern_equal(pattern1: &Pattern, pattern2: &Pattern) -> bool {
    if pattern1.get_vertices_num() == pattern2.get_vertices_num()
        && pattern1.get_edges_num() == pattern2.get_edges_num()
    {
        return pattern1.encode_to() == pattern2.encode_to();
    }

    false
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CostCount {
    instance_count: OrderedFloat<f64>,
    adjacency_count: OrderedFloat<f64>,
    intersect_count: OrderedFloat<f64>,
    left_join_count: OrderedFloat<f64>,
    right_join_count: OrderedFloat<f64>,
}

impl Display for CostCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let CostCount {
            instance_count,
            adjacency_count,
            intersect_count,
            left_join_count,
            right_join_count,
        } = self;
        write!(f, "({instance_count}, {adjacency_count}, {intersect_count}, {left_join_count}, {right_join_count})")
    }
}

impl CostCount {
    fn new(
        instance_count: OrderedFloat<f64>, adjacency_count: OrderedFloat<f64>,
        intersect_count: OrderedFloat<f64>, left_join_count: OrderedFloat<f64>,
        right_join_count: OrderedFloat<f64>,
    ) -> CostCount {
        CostCount { instance_count, adjacency_count, intersect_count, left_join_count, right_join_count }
    }

    fn from_src_pattern(src_pattern_count: OrderedFloat<f64>) -> CostCount {
        CostCount { instance_count: src_pattern_count, ..Default::default() }
    }

    fn from_extend(
        sub_pattern_count: OrderedFloat<f64>, pattern_count: OrderedFloat<f64>,
        adjacency_count: OrderedFloat<f64>, intersect_count: OrderedFloat<f64>, extend_num: usize,
    ) -> CostCount {
        let instance_count = sub_pattern_count
            + pattern_count
            + if extend_num <= 1 { OrderedFloat::default() } else { sub_pattern_count * extend_num as f64 };
        let adjacency_count = if extend_num <= 1 { OrderedFloat::default() } else { adjacency_count };
        // let instance_count = pattern_count
        //     + if extend_num <= 1 { OrderedFloat::default() } else { sub_pattern_count * extend_num as f64 };
        CostCount { instance_count, adjacency_count, intersect_count, ..Default::default() }
    }

    fn from_join(
        left_join_count: OrderedFloat<f64>, right_join_count: OrderedFloat<f64>,
        pattern_count: OrderedFloat<f64>,
    ) -> CostCount {
        CostCount { instance_count: pattern_count, left_join_count, right_join_count, ..Default::default() }
    }

    fn max_value() -> CostCount {
        CostCount::new(
            OrderedFloat::max_value(),
            OrderedFloat::max_value(),
            OrderedFloat::max_value(),
            OrderedFloat::max_value(),
            OrderedFloat::max_value(),
        )
    }

    fn get_cost(&self) -> OrderedFloat<f64> {
        if *self == CostCount::max_value() {
            OrderedFloat::max_value()
        } else {
            self.instance_count
                + (self.adjacency_count * (*ALPHA.read().unwrap()))
                + (self.intersect_count * (*BETA.read().unwrap()))
                + (self.left_join_count * (*W1.read().unwrap()))
                + (self.right_join_count * (*W1.read().unwrap()))
        }
    }
}

impl Add for CostCount {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        CostCount {
            instance_count: self.instance_count + rhs.instance_count,
            adjacency_count: self.adjacency_count + rhs.adjacency_count,
            intersect_count: self.intersect_count + rhs.intersect_count,
            left_join_count: self.left_join_count + rhs.left_join_count,
            right_join_count: self.right_join_count + rhs.left_join_count,
        }
    }
}

impl AddAssign for CostCount {
    fn add_assign(&mut self, rhs: Self) {
        self.instance_count += rhs.instance_count;
        self.adjacency_count += rhs.adjacency_count;
        self.intersect_count += rhs.intersect_count;
        self.left_join_count += rhs.left_join_count;
        self.right_join_count += rhs.right_join_count;
    }
}

impl PartialOrd for CostCount {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CostCount {
    fn cmp(&self, other: &Self) -> Ordering {
        self.get_cost().cmp(&other.get_cost())
    }
}

pub fn set_alpha(alpha: f64) {
    if let Ok(mut old_alpha) = ALPHA.write() {
        *old_alpha = alpha;
    }
}

pub fn set_beta(beta: f64) {
    if let Ok(mut old_beta) = BETA.write() {
        *old_beta = beta
    }
}

pub fn set_w1(w1: f64) {
    if let Ok(mut old_w1) = W1.write() {
        *old_w1 = w1
    }
}

pub fn set_w2(w2: f64) {
    if let Ok(mut old_w2) = W2.write() {
        *old_w2 = w2
    }
}

fn print_pattern_choose_approach_log(
    catalog: &Catalogue, pattern: &Pattern, pattern_index: NodeIndex, best_approach: Approach,
    min_cost: CostCount, cost_counts_vec: Vec<(NodeIndex, CostCount, CostCount, CostCount)>,
) {
    info!("Current Pattern: {}", pattern);
    info!("Current Pattern Index: {}", pattern_index.index());
    info!("-------------------------------------");
    for (pre_pattern_index, pre_pattern_cost, step_cost, cost) in cost_counts_vec {
        if pre_pattern_index == best_approach.get_src_pattern_index() {
            info!("This is the chosen Pre Pattern!");
        }
        let pre_pattern = catalog
            .get_pattern_weight(pre_pattern_index)
            .unwrap();
        info!("Pre Pattern: {}", pre_pattern.get_pattern());
        info!("Pre Pattern Index: {}", pre_pattern_index.index());
        info!("Pre Pattern CostCount: {}", pre_pattern_cost);
        info!("Pre Pattern Cost: {}", pre_pattern_cost.get_cost());
        info!("Step CostCount: {}", step_cost);
        info!("Step Cost: {}", step_cost.get_cost());
        info!("Pattern CostCount: {}", cost);
        info!("Pattern Cost: {}", cost.get_cost());
        info!("-------------------------------------");
    }
    info!("Chosen Pre Pattern Index: {}", best_approach.get_src_pattern_index().index());
    info!("Pattern Final CostCount: {}", min_cost);
    info!("Pattern Final Cost: {}\n", min_cost.get_cost());
}
