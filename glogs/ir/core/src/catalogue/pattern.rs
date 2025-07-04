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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::fmt::{Debug, Display};
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::iter::FromIterator;
use std::path::Path;

use ir_common::generated::algebra as pb;
use ir_common::generated::common as common_pb;
use ordered_float::OrderedFloat;
use serde::de::Visitor;
use serde::{Deserialize, Serialize};
use vec_map::VecMap;

use crate::catalogue::canonical_label::CanonicalLabelManager;
use crate::catalogue::extend_step::{
    get_subsets, limit_repeated_element_num, DefiniteExtendEdge, ExtendEdge, ExtendStep,
};
use crate::catalogue::pattern_meta::PatternMeta;
use crate::catalogue::{DynIter, PatternDirection, PatternId, PatternLabelId};
use crate::error::{IrError, IrResult};
use crate::plan::meta::{PlanMeta, TagId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PatternVertex {
    id: PatternId,
    label: PatternLabelId,
}

impl PatternVertex {
    pub fn new(id: PatternId, label: PatternLabelId) -> Self {
        PatternVertex { id, label }
    }

    #[inline]
    pub fn get_id(&self) -> PatternId {
        self.id
    }

    #[inline]
    pub fn get_label(&self) -> PatternLabelId {
        self.label
    }
}

/// Each PatternVertex of a Pattern has a related PatternVertexData struct
/// - These data heavily relies on Pattern and has no meaning without a Pattern
#[derive(Debug, Clone, Default)]
struct PatternVertexData {
    /// Identify whether two vertices are structurally equivalent in the pattern
    group: PatternId,
    /// DFS Rank ID assigned to the vertex during canonical labeling
    rank: PatternId,
    /// Outgoing adjacent edges and vertices related to this vertex
    out_adjacencies: Vec<Adjacency>,
    /// Incoming adjacent edges and vertices related to this vertex
    in_adjacencies: Vec<Adjacency>,
    /// Tag (alias) assigned to this vertex by user
    tag: Option<TagId>,
    /// Predicate(filter or other expressions) this vertex has
    predicate: Option<common_pb::Expression>,
}

#[derive(Debug, Clone)]
pub struct PatternEdge {
    id: PatternId,
    label: PatternLabelId,
    start_vertex: PatternVertex,
    end_vertex: PatternVertex,
}

impl PatternEdge {
    pub fn new(
        id: PatternId, label: PatternLabelId, start_vertex: PatternVertex, end_vertex: PatternVertex,
    ) -> PatternEdge {
        PatternEdge { id, label, start_vertex, end_vertex }
    }

    /// If the given direction is incoming, reverse the start and end vertex
    pub fn with_direction(mut self, direction: PatternDirection) -> PatternEdge {
        if direction == PatternDirection::In {
            std::mem::swap(&mut self.start_vertex, &mut self.end_vertex);
        }
        self
    }

    #[inline]
    pub fn get_id(&self) -> PatternId {
        self.id
    }

    #[inline]
    pub fn get_label(&self) -> PatternLabelId {
        self.label
    }

    #[inline]
    pub fn get_start_vertex(&self) -> PatternVertex {
        self.start_vertex
    }

    #[inline]
    pub fn get_end_vertex(&self) -> PatternVertex {
        self.end_vertex
    }
}

/// Each PatternEdge of a Pattern has a related PatternEdgeData struct
/// - These data heavily relies on Pattern and has no meaning without a Pattern
#[derive(Debug, Clone, Default)]
struct PatternEdgeData {
    /// DFS Rank ID assigned to the edge during canonical labeling
    rank: PatternId,
    /// Tag (alias) assigned to this edge by user
    tag: Option<TagId>,
    /// Predicate(filter or other expressions) this edge has
    predicate: Option<common_pb::Expression>,
}

/// Adjacency records a vertex's neighboring edge and vertex
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adjacency {
    /// the source vertex connect to the adjacent vertex through this edge
    edge_id: PatternId,
    /// connecting edge's label
    edge_label: PatternLabelId,
    /// the adjacent vertex
    adj_vertex: PatternVertex,
    /// the connecting direction: outgoing or incoming
    direction: PatternDirection,
}

impl Adjacency {
    fn new(src_vertex: &PatternVertex, edge: &PatternEdge) -> Option<Adjacency> {
        let start_vertex = edge.get_start_vertex();
        let end_vertex = edge.get_end_vertex();
        if (src_vertex.id, src_vertex.label) == (start_vertex.id, start_vertex.label) {
            Some(Adjacency {
                edge_id: edge.get_id(),
                edge_label: edge.get_label(),
                adj_vertex: edge.get_end_vertex(),
                direction: PatternDirection::Out,
            })
        } else if (src_vertex.id, src_vertex.label) == (end_vertex.id, end_vertex.label) {
            Some(Adjacency {
                edge_id: edge.get_id(),
                edge_label: edge.get_label(),
                adj_vertex: edge.get_start_vertex(),
                direction: PatternDirection::In,
            })
        } else {
            None
        }
    }

    #[inline]
    pub fn get_edge_id(&self) -> PatternId {
        self.edge_id
    }

    #[inline]
    pub fn get_edge_label(&self) -> PatternLabelId {
        self.edge_label
    }

    #[inline]
    pub fn get_adj_vertex(&self) -> PatternVertex {
        self.adj_vertex
    }

    #[inline]
    pub fn get_direction(&self) -> PatternDirection {
        self.direction
    }
}

#[derive(Debug, Clone, Default)]
pub struct Pattern {
    /// Key: edge id, Value: struct PatternEdge
    edges: VecMap<PatternEdge>,
    /// Key: vertex id, Value: the vertex in the pattern
    vertices: VecMap<PatternVertex>,
    /// Key: edge id, Value: struct PatternEdgeData
    /// - store data attaching to PatternEdge
    edges_data: VecMap<PatternEdgeData>,
    /// Key: vertex id, Value: struct PatternVertexData
    /// - store data attaching to PatternVertex
    vertices_data: VecMap<PatternVertexData>,
    /// Key: edge rank, Value: edge id
    /// - use an edge rank to locate an Edge
    rank_edge_map: VecMap<PatternId>,
    /// Key: vertex rank, Value: vertex id
    /// - use a vertex rank to locate a Vertex
    rank_vertex_map: VecMap<PatternId>,
    /// Key: edge's Tag info, Value: edge id
    /// - use a Tag to locate an Edge
    tag_edge_map: BTreeMap<TagId, PatternId>,
    /// Key: vertex's Tag info, Value: vertex id
    /// - use a Tag to locate a vertex
    tag_vertex_map: BTreeMap<TagId, PatternId>,
}

/// Initialze a Pattern from just a single Pattern Vertex
impl From<PatternVertex> for Pattern {
    fn from(vertex: PatternVertex) -> Pattern {
        Pattern {
            edges: VecMap::new(),
            vertices: VecMap::from_iter([(vertex.id, vertex)]),
            edges_data: VecMap::new(),
            vertices_data: VecMap::from_iter([(vertex.id, PatternVertexData::default())]),
            rank_edge_map: VecMap::new(),
            rank_vertex_map: VecMap::from_iter([(0, vertex.id)]),
            tag_edge_map: BTreeMap::new(),
            tag_vertex_map: BTreeMap::new(),
        }
    }
}

/// Initialize a Pattern from a vertor of Pattern Edges
impl TryFrom<Vec<PatternEdge>> for Pattern {
    type Error = IrError;

    fn try_from(edges: Vec<PatternEdge>) -> IrResult<Pattern> {
        if !edges.is_empty() {
            let mut new_pattern = Pattern::default();
            for edge in edges {
                // Add the new Pattern Edge to the new Pattern
                new_pattern
                    .edges
                    .insert(edge.get_id(), edge.clone());
                new_pattern
                    .edges_data
                    .insert(edge.get_id(), PatternEdgeData::default());
                // Add or update the start vertex to the new Pattern
                let start_vertex = new_pattern
                    .vertices
                    .entry(edge.get_start_vertex().get_id())
                    .or_insert(edge.get_start_vertex());
                // Update start vertex's outgoing info
                new_pattern
                    .vertices_data
                    .entry(start_vertex.get_id())
                    .or_insert(PatternVertexData::default())
                    .out_adjacencies
                    .push(Adjacency::new(start_vertex, &edge).unwrap());
                // Add or update the end vertex to the new Pattern
                let end_vertex = new_pattern
                    .vertices
                    .entry(edge.get_end_vertex().get_id())
                    .or_insert(edge.get_end_vertex());
                // Update end vertex's incoming info
                new_pattern
                    .vertices_data
                    .entry(end_vertex.get_id())
                    .or_insert(PatternVertexData::default())
                    .in_adjacencies
                    .push(Adjacency::new(end_vertex, &edge).unwrap());
            }
            new_pattern.canonical_labeling();
            Ok(new_pattern)
        } else {
            Err(IrError::InvalidPattern("Empty pattern".to_string()))
        }
    }
}

/// Initialize a Pattern from a protobuf Pattern
impl Pattern {
    pub fn from_pb_pattern(
        pb_pattern: &pb::Pattern, pattern_meta: &PatternMeta, plan_meta: &mut PlanMeta,
    ) -> IrResult<Pattern> {
        use pb::pattern::binder::Item as BinderItem;
        // next vertex id assign to the vertex picked from the pb pattern
        let mut next_vertex_id = plan_meta.get_max_tag_id() as PatternId;
        // next edge id assign to the edge picked from the pb pattern
        let mut next_edge_id = 0;
        // record the vertices from the pb pattern having tags
        let tag_set = get_all_tags_from_pb_pattern(pb_pattern)?;
        // record the label for each vertex from the pb pattern
        let mut v_id_label_maps: Vec<BTreeMap<PatternId, PatternLabelId>> = vec![];
        //
        let mut edges: Vec<(PatternId, PatternLabelId, PatternId, PatternId)> = vec![];
        // record the vertices from the pb pattern has predicates
        let mut v_id_predicate_map: BTreeMap<PatternId, common_pb::Expression> = BTreeMap::new();
        // record the edges from the pb pattern has predicates
        let mut e_id_predicate_map: BTreeMap<PatternId, common_pb::Expression> = BTreeMap::new();
        // record whether it is the first time to assign label
        let mut is_start = true;
        for sentence in &pb_pattern.sentences {
            if sentence.binders.is_empty() {
                return Err(IrError::MissingData("pb::Pattern::Sentence::binders".to_string()));
            }
            // pb pattern sentence must have start tag
            let start_tag = get_tag_from_name_or_id(
                sentence
                    .start
                    .clone()
                    .ok_or(IrError::MissingData("pb::Pattern::Sentence::start".to_string()))?,
            )?;
            // just use the start tag id as its pattern vertex id
            let start_tag_v_id = start_tag as PatternId;
            // it is allowed that the pb pattern sentence doesn't have an end tag
            let end_tag = if let Some(name_or_id) = sentence.end.clone() {
                Some(get_tag_from_name_or_id(name_or_id)?)
            } else {
                None
            };
            // if the end tag exists, just use the end tag id as its pattern vertex id
            let end_tag_v_id = end_tag.map(|tag| tag as PatternId);
            // record previous pattern edge's destinated vertex's id
            // init as start vertex's id
            let mut pre_dst_vertex_id: PatternId = start_tag_v_id;
            // find the first edge expand's index and last edge expand's index;
            let last_expand_index = get_sentence_last_expand_index(sentence);
            // iterate over the binders
            for (i, binder) in sentence.binders.iter().enumerate() {
                if let Some(BinderItem::Edge(edge_expand)) = binder.item.as_ref() {
                    // get edge label's id
                    let edge_label = get_edge_expand_label(edge_expand)?;
                    // assign the new pattern edge with a new id
                    let edge_id = assign_id(&mut next_edge_id, None);
                    // get edge direction
                    let edge_direction = PatternDirection::try_from(edge_expand.direction)?;
                    // add edge predicate
                    if let Some(expr) = get_edge_expand_predicate(edge_expand) {
                        e_id_predicate_map.insert(edge_id, expr.clone());
                    }
                    // assign/pick the souce vertex id and destination vertex id of the pattern edge
                    let src_vertex_id = pre_dst_vertex_id;
                    let dst_vertex_id = assign_expand_dst_vertex_id(
                        i == last_expand_index.unwrap(),
                        end_tag_v_id,
                        edge_expand,
                        &tag_set,
                        &mut next_vertex_id,
                    )?;
                    pre_dst_vertex_id = dst_vertex_id;
                    // assign vertices labels
                    let src_dst_v_id_label_map = get_src_dst_vertex_id_label_maps(
                        pattern_meta,
                        edge_label,
                        edge_direction,
                        src_vertex_id,
                        dst_vertex_id,
                    );
                    v_id_label_maps =
                        join_id_label_maps(v_id_label_maps, src_dst_v_id_label_map, &mut is_start);
                    if let PatternDirection::Out = edge_direction {
                        edges.push((edge_id, edge_label, src_vertex_id, dst_vertex_id));
                    } else {
                        edges.push((edge_id, edge_label, dst_vertex_id, src_vertex_id));
                    }
                } else if let Some(BinderItem::Select(select)) = binder.item.as_ref() {
                    if let Some(predicate) = select.predicate.as_ref() {
                        if let Some(v_id_label_map) =
                            pick_id_label_map_from_predicate(pre_dst_vertex_id, predicate)
                        {
                            v_id_label_maps =
                                join_id_label_maps(v_id_label_maps, vec![v_id_label_map], &mut is_start);
                        } else {
                            v_id_predicate_map.insert(pre_dst_vertex_id, predicate.clone());
                        }
                    }
                } else {
                    return Err(IrError::MissingData("pb::pattern::binder::Item".to_string()));
                }
            }
        }
        if v_id_label_maps.is_empty() {
            return Err(IrError::InvalidPattern("The pattern is illegal according to schema".to_string()));
        } else if v_id_label_maps.len() > 1 {
            return Err(IrError::Unsupported("Fuzzy Pattern".to_string()));
        }
        let v_id_label_map = v_id_label_maps.remove(0);
        let pattern_edges: Vec<PatternEdge> = edges
            .into_iter()
            .map(|(e_id, e_label, start_v_id, end_v_id)| {
                let start_v_label = *v_id_label_map.get(&start_v_id).unwrap();
                let end_v_label = *v_id_label_map.get(&end_v_id).unwrap();
                PatternEdge::new(
                    e_id,
                    e_label,
                    PatternVertex::new(start_v_id, start_v_label),
                    PatternVertex::new(end_v_id, end_v_label),
                )
            })
            .collect();
        plan_meta.set_max_tag_id(next_vertex_id as TagId);
        Pattern::try_from(pattern_edges).map(|mut pattern| {
            for tag in tag_set {
                pattern.set_vertex_tag(tag as PatternId, tag);
            }
            for (v_id, predicate) in v_id_predicate_map {
                pattern.set_vertex_predicate(v_id, predicate);
            }
            for (e_id, predicate) in e_id_predicate_map {
                pattern.set_edge_predicate(e_id, predicate);
            }
            pattern
        })
    }
}

/// Get the tag info from the given name_or_id
/// - in pb::Pattern transformation, tag is required to be id instead of str
fn get_tag_from_name_or_id(name_or_id: common_pb::NameOrId) -> IrResult<TagId> {
    let tag: ir_common::NameOrId = name_or_id.try_into()?;
    match tag {
        ir_common::NameOrId::Id(tag_id) => Ok(tag_id as TagId),
        _ => Err(IrError::TagNotExist(tag)),
    }
}

/// Get all the tags from the pb Pattern and store in a set
fn get_all_tags_from_pb_pattern(pb_pattern: &pb::Pattern) -> IrResult<BTreeSet<TagId>> {
    use pb::pattern::binder::Item as BinderItem;
    let mut tag_id_set = BTreeSet::new();
    for sentence in pb_pattern.sentences.iter() {
        if let Some(start_tag) = sentence.start.as_ref().cloned() {
            let start_tag_id = get_tag_from_name_or_id(start_tag)?;
            tag_id_set.insert(start_tag_id);
        }
        if let Some(end_tag) = sentence.end.as_ref().cloned() {
            let end_tag_id = get_tag_from_name_or_id(end_tag)?;
            tag_id_set.insert(end_tag_id);
        }
        for binder in sentence.binders.iter() {
            if let Some(BinderItem::Edge(edge_expand)) = binder.item.as_ref() {
                if let Some(tag) = edge_expand.alias.as_ref().cloned() {
                    let tag_id = get_tag_from_name_or_id(tag)?;
                    tag_id_set.insert(tag_id);
                }
            }
        }
    }
    Ok(tag_id_set)
}

/// Get the last edge expand's index of a pb pattern sentence among all of its binders
fn get_sentence_last_expand_index(sentence: &pb::pattern::Sentence) -> Option<usize> {
    sentence
        .binders
        .iter()
        .enumerate()
        .rev()
        .find(|(_, binder)| matches!(binder.item.as_ref(), Some(pb::pattern::binder::Item::Edge(_))))
        .map(|(id, _)| id)
}

/// Get the edge expand's label
/// - in current realization, edge_expand only allows to have one label
/// - if it has no label or more than one label, give Error
fn get_edge_expand_label(edge_expand: &pb::EdgeExpand) -> IrResult<PatternLabelId> {
    if edge_expand.expand_opt != pb::edge_expand::ExpandOpt::Vertex as i32 {
        return Err(IrError::Unsupported("Expand only edge in pattern".to_string()));
    }
    if let Some(params) = edge_expand.params.as_ref() {
        // TODO: Support Fuzzy Pattern
        if params.tables.is_empty() {
            return Err(IrError::Unsupported("FuzzyPattern: no specific edge expand label".to_string()));
        } else if params.tables.len() > 1 {
            return Err(IrError::Unsupported("FuzzyPattern: more than 1 edge expand label".to_string()));
        }
        // get edge label's id
        match params.tables[0].item.as_ref() {
            Some(common_pb::name_or_id::Item::Id(e_label_id)) => Ok(*e_label_id),
            _ => Err(IrError::InvalidPattern("edge expand doesn't have valid label".to_string())),
        }
    } else {
        Err(IrError::MissingData("pb::EdgeExpand.params".to_string()))
    }
}

/// Get the predicate(if it has) of the edge expand
fn get_edge_expand_predicate(edge_expand: &pb::EdgeExpand) -> Option<common_pb::Expression> {
    if let Some(params) = edge_expand.params.as_ref() {
        params.predicate.clone()
    } else {
        None
    }
}

/// Assign a vertex or edge with the next_id, and add the next_id by one
/// - For a vertex:
/// - - if the vertex has tag, just use its tag id as the pattern id
/// - - the next_id cannot be the same as another vertex's tag id (pattern id)
/// - - otherwise the assigned pattern id will be repeated
fn assign_id(next_id: &mut PatternId, tag_set_opt: Option<&BTreeSet<TagId>>) -> PatternId {
    if let Some(tag_set) = tag_set_opt {
        while tag_set.contains(&(*next_id as TagId)) {
            *next_id += 1;
        }
    }
    let id_to_assign = *next_id;
    *next_id += 1;
    id_to_assign
}

/// Assign an id the dst vertex of an edge expand
/// - firstly, check whether the edge expand is the tail of the sentence or not
///   - if it is sentence's end vertex
///     - if the sentence's end vertex's id is already assigned, just use it
///     - else, assign it with a new id
///   - else
///     - if the dst vertex is related with the tag, assign its id by tag
///     - else, assign it with a new id
fn assign_expand_dst_vertex_id(
    is_tail: bool, sentence_end_id: Option<PatternId>, edge_expand: &pb::EdgeExpand,
    tag_set: &BTreeSet<TagId>, next_vertex_id: &mut PatternId,
) -> IrResult<PatternId> {
    if is_tail {
        if let Some(v_id) = sentence_end_id {
            Ok(v_id)
        } else {
            Ok(assign_id(next_vertex_id, Some(tag_set)))
        }
    } else {
        // check alias tag
        let dst_vertex_tag = if let Some(name_or_id) = edge_expand.alias.clone() {
            Some(get_tag_from_name_or_id(name_or_id)?)
        } else {
            None
        };
        // if the dst vertex has tag, just use the tag id as its pattern id
        if let Some(tag) = dst_vertex_tag {
            Ok(tag as PatternId)
        } else {
            Ok(assign_id(next_vertex_id, Some(tag_set)))
        }
    }
}

fn get_src_dst_vertex_id_label_maps(
    pattern_meta: &PatternMeta, edge_label: PatternLabelId, edge_direction: PatternDirection,
    src_vertex_id: PatternId, dst_vertex_id: PatternId,
) -> Vec<BTreeMap<PatternId, PatternLabelId>> {
    match edge_direction {
        PatternDirection::Out => pattern_meta
            .associated_vlabels_iter_by_elabel(edge_label)
            .map(|(start_v_label, end_v_label)| {
                BTreeMap::from_iter([(src_vertex_id, start_v_label), (dst_vertex_id, end_v_label)])
            })
            .collect(),
        PatternDirection::In => pattern_meta
            .associated_vlabels_iter_by_elabel(edge_label)
            .map(|(start_v_label, end_v_label)| {
                BTreeMap::from_iter([(src_vertex_id, end_v_label), (dst_vertex_id, start_v_label)])
            })
            .collect(),
    }
}

fn pick_id_label_map_from_predicate(
    pre_dst_vertex_id: PatternId, predicate: &common_pb::Expression,
) -> Option<BTreeMap<PatternId, PatternLabelId>> {
    if predicate.operators.len() == 3 {
        let is_label_select = predicate
            .operators
            .first()
            .and_then(|opr| opr.item.as_ref())
            .and_then(|item| {
                if let common_pb::expr_opr::Item::Var(var) = item {
                    var.property.as_ref()
                } else {
                    None
                }
            })
            .and_then(|property| property.item.as_ref())
            .and_then(|item| if let common_pb::property::Item::Label(_) = item { Some(()) } else { None })
            .is_some();
        let is_equal = predicate
            .operators
            .get(1)
            .and_then(|opr| opr.item.as_ref())
            .and_then(|item| if let common_pb::expr_opr::Item::Logical(0) = item { Some(()) } else { None })
            .is_some();
        let label_id = predicate
            .operators
            .get(2)
            .and_then(|opr| opr.item.as_ref())
            .and_then(
                |item| if let common_pb::expr_opr::Item::Const(value) = item { Some(value) } else { None },
            )
            .and_then(|value| {
                if let Some(common_pb::value::Item::I64(label_id)) = value.item {
                    Some(label_id as PatternLabelId)
                } else if let Some(common_pb::value::Item::I32(label_id)) = value.item {
                    Some(label_id)
                } else {
                    None
                }
            });
        if !is_label_select || !is_equal {
            return None;
        }
        label_id.map(|label_id| BTreeMap::from_iter([(pre_dst_vertex_id, label_id)]))
    } else {
        None
    }
}

fn join_id_label_maps(
    mut left_maps: Vec<BTreeMap<PatternId, PatternLabelId>>,
    mut right_maps: Vec<BTreeMap<PatternId, PatternLabelId>>, is_start: &mut bool,
) -> Vec<BTreeMap<PatternId, PatternLabelId>> {
    if left_maps.is_empty() || right_maps.is_empty() {
        if *is_start {
            *is_start = false;
            if left_maps.is_empty() {
                return right_maps;
            } else {
                return left_maps;
            }
        } else {
            return vec![];
        }
    }
    if left_maps.len() > right_maps.len() {
        (left_maps, right_maps) = (right_maps, left_maps);
    }
    let first_left_map = left_maps.first().unwrap();
    let first_right_map = right_maps.first().unwrap();
    let mut join_keys = vec![];
    for vertex_id in first_left_map.keys() {
        if first_right_map.contains_key(vertex_id) {
            join_keys.push(*vertex_id);
        }
    }
    let mut joined_maps = vec![];
    for left_map in left_maps {
        let left_join_values: Vec<PatternLabelId> = join_keys
            .iter()
            .map(|key| *left_map.get(key).unwrap())
            .collect();
        for right_map in right_maps.iter() {
            let right_join_values: Vec<PatternLabelId> = join_keys
                .iter()
                .map(|key| *right_map.get(key).unwrap())
                .collect();
            if left_join_values == right_join_values {
                let mut new_map = left_map.clone();
                for (key, value) in right_map.iter() {
                    new_map.insert(*key, *value);
                }
                joined_maps.push(new_map);
            }
        }
    }
    joined_maps
}

/// Getters of fields of Pattern
impl Pattern {
    /// Get a PatternEdge struct from an edge id
    #[inline]
    pub fn get_edge(&self, edge_id: PatternId) -> Option<&PatternEdge> {
        self.edges.get(edge_id)
    }

    /// Get PatternEdge from Given Edge Rank
    #[inline]
    pub fn get_edge_from_rank(&self, edge_rank: PatternId) -> Option<&PatternEdge> {
        self.rank_edge_map
            .get(edge_rank)
            .and_then(|&edge_id| self.get_edge(edge_id))
    }

    /// Get PatternEdge from Given Tag
    #[inline]
    pub fn get_edge_from_tag(&self, edge_tag: TagId) -> Option<&PatternEdge> {
        self.tag_edge_map
            .get(&edge_tag)
            .and_then(|&edge_id| self.get_edge(edge_id))
    }

    /// Get the total number of edges in the pattern
    #[inline]
    pub fn get_edges_num(&self) -> usize {
        self.edges.len()
    }

    #[inline]
    pub fn get_min_edge_id(&self) -> PatternId {
        self.edges
            .iter()
            .map(|(edge_id, _)| edge_id)
            .next()
            .unwrap_or(0)
    }

    #[inline]
    pub fn get_max_edge_id(&self) -> PatternId {
        self.edges
            .iter()
            .map(|(edge_id, _)| edge_id)
            .last()
            .unwrap_or(0)
    }

    /// Get the minimum edge label id of the current pattern
    #[inline]
    pub fn get_min_edge_label(&self) -> Option<PatternLabelId> {
        self.edges
            .iter()
            .map(|(_, edge)| edge.get_label())
            .min()
    }

    /// Get the maximum edge label id of the current pattern
    #[inline]
    pub fn get_max_edge_label(&self) -> Option<PatternLabelId> {
        self.edges
            .iter()
            .map(|(_, edge)| edge.get_label())
            .max()
    }

    /// Get a PatternEdge's Rank info
    #[inline]
    pub fn get_edge_rank(&self, edge_id: PatternId) -> Option<PatternId> {
        self.edges_data
            .get(edge_id)
            .map(|edge_data| edge_data.rank)
    }

    /// Get a PatternEdge's Tag info
    #[inline]
    pub fn get_edge_tag(&self, edge_id: PatternId) -> Option<TagId> {
        self.edges_data
            .get(edge_id)
            .and_then(|edge_data| edge_data.tag)
    }

    /// Get the predicate requirement of a PatternEdge
    #[inline]
    pub fn get_edge_predicate(&self, edge_id: PatternId) -> Option<&common_pb::Expression> {
        self.edges_data
            .get(edge_id)
            .and_then(|edge_data| edge_data.predicate.as_ref())
    }

    /// Get a PatternVertex struct from a vertex id
    #[inline]
    pub fn get_vertex(&self, vertex_id: PatternId) -> Option<&PatternVertex> {
        self.vertices.get(vertex_id)
    }

    /// Get PatternVertex Reference from Given Rank
    #[inline]
    pub fn get_vertex_from_rank(&self, vertex_rank: PatternId) -> Option<&PatternVertex> {
        self.rank_vertex_map
            .get(vertex_rank)
            .and_then(|&vertex_id| self.get_vertex(vertex_id))
    }

    /// Get PatternVertex Reference from Given Tag
    #[inline]
    pub fn get_vertex_from_tag(&self, vertex_tag: TagId) -> Option<&PatternVertex> {
        self.tag_vertex_map
            .get(&vertex_tag)
            .and_then(|&vertex_id| self.get_vertex(vertex_id))
    }

    /// Get the total number of vertices in the pattern
    #[inline]
    pub fn get_vertices_num(&self) -> usize {
        self.vertices.len()
    }

    #[inline]
    pub fn get_min_vertex_id(&self) -> PatternId {
        self.vertices
            .iter()
            .map(|(vertex_id, _)| vertex_id)
            .next()
            .unwrap()
    }

    #[inline]
    pub fn get_max_vertex_id(&self) -> PatternId {
        self.vertices
            .iter()
            .map(|(vertex_id, _)| vertex_id)
            .last()
            .unwrap()
    }

    /// Get the minimum vertex label id of the current pattern
    #[inline]
    pub fn get_min_vertex_label(&self) -> Option<PatternLabelId> {
        self.vertices
            .iter()
            .map(|(_, vertex)| vertex.get_label())
            .min()
    }

    /// Get the maximum vertex label id of the current pattern
    pub fn get_max_vertex_label(&self) -> Option<PatternLabelId> {
        self.vertices
            .iter()
            .map(|(_, vertex)| vertex.get_label())
            .max()
    }

    /// Get Vertex Rank from Vertex ID Reference
    #[inline]
    pub fn get_vertex_group(&self, vertex_id: PatternId) -> Option<PatternId> {
        self.vertices_data
            .get(vertex_id)
            .map(|vertex_data| vertex_data.group)
    }

    /// Get Vertex Rank from Vertex ID Reference
    #[inline]
    pub fn get_vertex_rank(&self, vertex_id: PatternId) -> Option<PatternId> {
        self.vertices_data
            .get(vertex_id)
            .map(|vertex_data| vertex_data.rank)
    }

    /// Get a PatternVertex's Tag info
    #[inline]
    pub fn get_vertex_tag(&self, vertex_id: PatternId) -> Option<TagId> {
        self.vertices_data
            .get(vertex_id)
            .and_then(|vertex_data| vertex_data.tag)
    }

    /// Get the predicate requirement of a PatternVertex
    #[inline]
    pub fn get_vertex_predicate(&self, vertex_id: PatternId) -> Option<&common_pb::Expression> {
        self.vertices_data
            .get(vertex_id)
            .and_then(|vertex_data| vertex_data.predicate.as_ref())
    }

    /// Count how many outgoing edges connect to this vertex
    #[inline]
    pub fn get_vertex_out_degree(&self, vertex_id: PatternId) -> usize {
        self.vertices_data
            .get(vertex_id)
            .map(|vertex_data| vertex_data.out_adjacencies.len())
            .unwrap_or(0)
    }

    /// Count how many incoming edges connect to this vertex
    #[inline]
    pub fn get_vertex_in_degree(&self, vertex_id: PatternId) -> usize {
        self.vertices_data
            .get(vertex_id)
            .map(|vertex_data| vertex_data.in_adjacencies.len())
            .unwrap_or(0)
    }

    /// Count how many edges connect to this vertex
    #[inline]
    pub fn get_vertex_degree(&self, vertex_id: PatternId) -> usize {
        self.get_vertex_out_degree(vertex_id) + self.get_vertex_in_degree(vertex_id)
    }

    #[inline]
    pub fn get_vertices_predicate_num(&self) -> usize {
        self.vertices_iter()
            .map(|v| if self.get_vertex_predicate(v.get_id()).is_some() { 1 } else { 0 })
            .sum()
    }

    #[inline]
    pub fn get_edges_predicate_num(&self) -> usize {
        self.edges_iter()
            .map(|e| if self.get_vertex_predicate(e.get_id()).is_some() { 1 } else { 0 })
            .sum()
    }

    #[inline]
    pub fn get_predicate_num(&self) -> usize {
        self.get_vertices_predicate_num() + self.get_edges_predicate_num()
    }
}

/// Iterators of fields of Pattern
impl Pattern {
    /// Iterate Edges
    pub fn edges_iter(&self) -> DynIter<&PatternEdge> {
        Box::new(self.edges.iter().map(|(_, edge)| edge))
    }

    /// Iterate Edges with the given edge label
    pub fn edges_iter_by_label(&self, edge_label: PatternLabelId) -> DynIter<&PatternEdge> {
        Box::new(
            self.edges
                .iter()
                .map(|(_, edge)| edge)
                .filter(move |edge| edge.get_label() == edge_label),
        )
    }

    /// Iterate over edges that has tag
    pub fn edges_with_tag_iter(&self) -> DynIter<&PatternEdge> {
        Box::new(
            self.tag_edge_map
                .iter()
                .map(move |(_, &edge_id)| self.get_edge(edge_id).unwrap()),
        )
    }

    /// Iterate Vertices
    pub fn vertices_iter(&self) -> DynIter<&PatternVertex> {
        Box::new(self.vertices.iter().map(|(_, vertex)| vertex))
    }

    /// Iterate Vertices with the given vertex label
    pub fn vertices_iter_by_label(&self, vertex_label: PatternLabelId) -> DynIter<&PatternVertex> {
        Box::new(
            self.vertices
                .iter()
                .map(|(_, vertex)| vertex)
                .filter(move |vertex| vertex.get_label() == vertex_label),
        )
    }

    /// Iterate over vertices that has tag
    pub fn vertices_with_tag_iter(&self) -> DynIter<&PatternVertex> {
        Box::new(
            self.tag_vertex_map
                .iter()
                .map(move |(_, &vertex_id)| self.get_vertex(vertex_id).unwrap()),
        )
    }

    /// Iterate all outgoing edges from the given vertex
    pub fn out_adjacencies_iter(&self, vertex_id: PatternId) -> DynIter<&Adjacency> {
        if let Some(vertex_data) = self.vertices_data.get(vertex_id) {
            Box::new(vertex_data.out_adjacencies.iter())
        } else {
            Box::new(std::iter::empty())
        }
    }

    /// Iterate all incoming edges to the given vertex
    pub fn in_adjacencies_iter(&self, vertex_id: PatternId) -> DynIter<&Adjacency> {
        if let Some(vertex_data) = self.vertices_data.get(vertex_id) {
            Box::new(vertex_data.in_adjacencies.iter())
        } else {
            Box::new(std::iter::empty())
        }
    }

    /// Iterate both outgoing and incoming edges of the given vertex
    pub fn adjacencies_iter(&self, vertex_id: PatternId) -> DynIter<&Adjacency> {
        Box::new(
            self.out_adjacencies_iter(vertex_id)
                .chain(self.in_adjacencies_iter(vertex_id)),
        )
    }
}

impl Display for PatternVertex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}:{})", self.id, self.label)
    }
}

impl Display for PatternEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-[{}:{}]->{}", self.start_vertex, self.id, self.label, self.end_vertex)
    }
}

impl Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.get_vertices_num() {
            0 => write!(f, ""),
            1 => {
                let (_, v) = self.vertices.iter().next().unwrap();
                write!(f, "{}", v)
            }
            _ => {
                let edges: Vec<_> = self
                    .edges
                    .values()
                    .map(|e| format!("{}", e))
                    .collect();
                write!(f, "{}", edges.join(", "))
            }
        }
    }
}

/// Setters of fields of Pattern
impl Pattern {
    /// Assign a PatternEdge with the given group
    fn set_edge_rank(&mut self, edge_id: PatternId, edge_rank: PatternId) {
        // Assign the rank to the edge
        if let Some(edge_data) = self.edges_data.get_mut(edge_id) {
            self.rank_edge_map.insert(edge_rank, edge_id);
            edge_data.rank = edge_rank;
        }
    }

    /// Assign a PatternEdge of the Pattern with the Given Tag
    pub fn set_edge_tag(&mut self, edge_tag: TagId, edge_id: PatternId) {
        // If the tag is previously assigned to another edge, remove it
        if let Some(&old_edge_id) = self.tag_edge_map.get(&edge_tag) {
            self.edges_data
                .get_mut(old_edge_id)
                .unwrap()
                .tag = None;
        }
        // Assign the tag to the edge
        if let Some(edge_data) = self.edges_data.get_mut(edge_id) {
            self.tag_edge_map.insert(edge_tag, edge_id);
            edge_data.tag = Some(edge_tag);
        }
    }

    /// Set predicate requirement of a PatternEdge
    pub fn set_edge_predicate(&mut self, edge_id: PatternId, predicate: common_pb::Expression) {
        if let Some(edge_data) = self.edges_data.get_mut(edge_id) {
            edge_data.predicate = Some(predicate);
        }
    }

    /// Assign a PatternVertex with the given group
    fn set_vertex_group(&mut self, vertex_id: PatternId, group: PatternId) {
        if let Some(vertex_data) = self.vertices_data.get_mut(vertex_id) {
            vertex_data.group = group;
        }
    }

    fn set_vertex_rank(&mut self, vertex_id: PatternId, vertex_rank: PatternId) {
        // Assign the rank to the vertex
        if let Some(vertex_data) = self.vertices_data.get_mut(vertex_id) {
            self.rank_vertex_map
                .insert(vertex_rank, vertex_id);
            vertex_data.rank = vertex_rank;
        }
    }

    /// Assign a PatternVertex with the given tag
    pub fn set_vertex_tag(&mut self, vertex_id: PatternId, vertex_tag: TagId) {
        // If the tag is previously assigned to another vertex, remove it
        if let Some(&old_vertex_id) = self.tag_vertex_map.get(&vertex_tag) {
            self.vertices_data
                .get_mut(old_vertex_id)
                .unwrap()
                .tag = None;
        }
        // Assign the tag to the vertex
        if let Some(vertex_data) = self.vertices_data.get_mut(vertex_id) {
            self.tag_vertex_map
                .insert(vertex_tag, vertex_id);
            vertex_data.tag = Some(vertex_tag);
        }
    }

    /// Set predicate requirement of a PatternVertex
    pub fn set_vertex_predicate(&mut self, vertex_id: PatternId, predicate: common_pb::Expression) {
        if let Some(vertex_data) = self.vertices_data.get_mut(vertex_id) {
            vertex_data.predicate = Some(predicate);
        }
    }
}

/// Methods for Canonical Labeling
impl Pattern {
    /// Canonical Labeling gives each vertex a unique ID (rank), which is used to encode the pattern.
    ///
    /// It consists of two parts:
    /// - Vertex Grouping (Partition): vertices in the same group (partition) are equivalent in structure.
    /// - Pattern Ranking: given the vertex groups, rank each vertex and edge with a unique ID.
    fn canonical_labeling(&mut self) {
        let mut canonical_label_manager = CanonicalLabelManager::from(&*self);
        canonical_label_manager.vertex_grouping(self);
        canonical_label_manager.pattern_ranking(self);
        self.update_vertex_groups(&canonical_label_manager);
        self.update_pattern_ranks(&canonical_label_manager);
    }

    /// Update vertex groups
    fn update_vertex_groups(&mut self, canonical_label_manager: &CanonicalLabelManager) {
        canonical_label_manager
            .vertex_groups_iter()
            .for_each(|(v_id, v_group)| {
                self.set_vertex_group(v_id, v_group);
            });
    }

    /// Update ranks for vertices and edges
    fn update_pattern_ranks(&mut self, canonical_label_manager: &CanonicalLabelManager) {
        // update vertex ranks
        self.rank_vertex_map.clear();
        canonical_label_manager
            .vertex_ranks_iter()
            .for_each(|(v_id, v_rank)| {
                self.set_vertex_rank(
                    v_id,
                    v_rank.expect("Error occurs when ranking vertices: Not all vertices are ranked"),
                );
            });

        // Update edge ranks
        self.rank_edge_map.clear();
        canonical_label_manager
            .edge_ranks_iter()
            .for_each(|(e_id, e_rank)| {
                self.set_edge_rank(
                    e_id,
                    e_rank.expect("Error occurs when ranking edges: Not all edges are ranked"),
                );
            });
    }

    /// Return the number of connected components in a pattern
    ///
    /// Pattern is disconnected if number of connected components is bigger than 1
    pub fn get_connected_component_num(&self) -> usize {
        self.vertices_iter()
            .map(|vertex| vertex.get_id())
            .filter(|&v_id| self.get_vertex_rank(v_id).unwrap() == 0)
            .count()
    }

    /// Determine whther a pattern is connected or not
    ///
    /// The edge ranks are used since only disconnected pattern could result in some edges with None rank.
    pub fn is_connected(&self) -> bool {
        self.get_connected_component_num() == 1
    }

    pub fn get_connected_components(&self) -> Vec<Pattern> {
        // // ---debug---
        // self.vertices_iter()
        //     .for_each(|vertex| {
        //         let v_id = vertex.get_id();
        //         let v_label = vertex.get_label();
        //         let v_rank = self.get_vertex_rank(v_id).expect("Failed to get vertex rank from id");
        //         println!("ID: {v_id}, Label: {v_label}, Rank: {v_rank}");
        //     });
        // // ---debug---
        let mut visited_vertices: BTreeSet<PatternId> = BTreeSet::new();
        let connected_components: Vec<Pattern> = self
            .vertices_iter()
            .map(|vertex| vertex.get_id())
            .filter(|&v_id| self.get_vertex_rank(v_id).unwrap() == 0)
            .map(|v_id| {
                // BFS to traverse the connected component
                let mut vertices_queue: VecDeque<PatternId> = VecDeque::new();
                let mut pattern_edges: Vec<PatternEdge> = vec![];
                vertices_queue.push_back(v_id);
                while let Some(current_v_id) = vertices_queue.pop_front() {
                    let current_v_label: PatternLabelId = self
                        .get_vertex(current_v_id)
                        .expect("Vertex Not Found")
                        .get_label();
                    let traversed_vertices: Vec<PatternId> = self
                        .adjacencies_iter(current_v_id)
                        .filter(|&adj| !visited_vertices.contains(&adj.get_adj_vertex().get_id()))
                        .map(|adj| {
                            let adj_v_id: PatternId = adj.get_adj_vertex().get_id();
                            let adj_v_label: PatternLabelId = adj.get_adj_vertex().get_label();
                            let e_id: PatternId = adj.get_edge_id();
                            let e_label: PatternLabelId = adj.get_edge_label();
                            let e_direction: PatternDirection = adj.get_direction();
                            let (start_v_id, end_v_id) = match e_direction {
                                PatternDirection::Out => (current_v_id, adj_v_id),
                                PatternDirection::In => (adj_v_id, current_v_id),
                            };
                            let (start_v_label, end_v_label) = match e_direction {
                                PatternDirection::Out => (current_v_label, adj_v_label),
                                PatternDirection::In => (adj_v_label, current_v_label),
                            };
                            let start_vertex = PatternVertex::new(start_v_id, start_v_label);
                            let end_vertex = PatternVertex::new(end_v_id, end_v_label);
                            pattern_edges.push(PatternEdge::new(e_id, e_label, start_vertex, end_vertex));
                            // Push unvisited vertex to vertices queue
                            vertices_queue.push_back(adj_v_id);
                            current_v_id
                        })
                        .collect();
                    // Mark traversed vertices as visited
                    traversed_vertices.iter().for_each(|&v_id| {
                        visited_vertices.insert(v_id);
                    });
                }

                // Build pattern
                if pattern_edges.is_empty() {
                    // Case-1: Single Vertex
                    let v_label: PatternLabelId = self
                        .get_vertex(v_id)
                        .expect("Vertex Not Found in Pattern")
                        .get_label();
                    Pattern::from(PatternVertex::new(v_id, v_label))
                } else {
                    // Case-2: Pattern Edges
                    Pattern::try_from(pattern_edges).expect("Failed to build pattern from a single vertex")
                }
            })
            .collect();

        connected_components
    }
}

/// Methods for Pattern Edit
impl Pattern {
    /// Get all the vertices(id) with the same vertex label and vertex group
    ///
    /// These vertices are equivalent in the Pattern
    pub fn get_equivalent_vertices(
        &self, v_label: PatternLabelId, v_group: PatternId,
    ) -> Vec<PatternVertex> {
        self.vertices_iter()
            .filter(|vertex| {
                vertex.get_label() == v_label && self.get_vertex_group(vertex.get_id()).unwrap() == v_group
            })
            .cloned()
            .collect()
    }

    /// Extend the current Pattern to a new Pattern with the given ExtendStep
    /// - If the ExtendStep is not matched with the current Pattern, the function will return None
    /// - Else, it will return the new Pattern after the extension
    pub fn extend(&self, extend_step: &ExtendStep) -> Option<Pattern> {
        let mut new_pattern = self.clone();
        let target_vertex_label = extend_step.get_target_vertex_label();
        let target_vertex = PatternVertex::new(self.get_max_vertex_id() + 1, target_vertex_label);
        let target_vertex_data = PatternVertexData::default();
        // Add the newly extended pattern vertex to the new pattern
        new_pattern
            .vertices
            .insert(target_vertex.get_id(), target_vertex);
        new_pattern
            .vertices_data
            .insert(target_vertex.get_id(), target_vertex_data);
        // Iterately add the new pattern edges to the new pattern
        for extend_edge in extend_step.iter() {
            let src_vertex_rank = extend_edge.get_src_vertex_rank();
            if let Some(src_vertex) = self
                .get_vertex_from_rank(src_vertex_rank)
                .cloned()
            {
                let new_pattern_edge_id = new_pattern.get_max_edge_id() + 1;
                let new_pattern_edge_label = extend_edge.get_edge_label();
                let (mut start_vertex, mut end_vertex) = (src_vertex, target_vertex);
                if let PatternDirection::In = extend_edge.get_direction() {
                    std::mem::swap(&mut start_vertex, &mut end_vertex);
                }
                let new_pattern_edge =
                    PatternEdge::new(new_pattern_edge_id, new_pattern_edge_label, start_vertex, end_vertex);
                // Update start vertex and end vertex's adjacency info
                let start_vertex_new_adjacency = Adjacency::new(&start_vertex, &new_pattern_edge).unwrap();
                new_pattern
                    .vertices_data
                    .get_mut(start_vertex.get_id())
                    .unwrap()
                    .out_adjacencies
                    .push(start_vertex_new_adjacency);
                let end_vertex_new_adjacency = Adjacency::new(&end_vertex, &new_pattern_edge).unwrap();
                new_pattern
                    .vertices_data
                    .get_mut(end_vertex.get_id())
                    .unwrap()
                    .in_adjacencies
                    .push(end_vertex_new_adjacency);
                new_pattern
                    .edges
                    .insert(new_pattern_edge_id, new_pattern_edge);
                new_pattern
                    .edges_data
                    .insert(new_pattern_edge_id, PatternEdgeData::default());
            } else {
                return None;
            }
        }

        new_pattern.canonical_labeling();
        Some(new_pattern)
    }

    /// Find all possible ExtendSteps of current pattern based on the given Pattern Meta
    pub fn get_extend_steps(
        &self, pattern_meta: &PatternMeta, same_label_vertex_limit: usize,
    ) -> Vec<ExtendStep> {
        let mut extend_steps = vec![];
        // Get all vertex labels from pattern meta as the possible extend target vertex
        let target_v_labels = pattern_meta.vertex_label_ids_iter();
        // For every possible extend target vertex label, find its all adjacent edges to the current pattern
        // Count each vertex's label number
        let mut vertex_label_count_map: HashMap<PatternLabelId, usize> = HashMap::new();
        for vertex in self.vertices_iter() {
            *vertex_label_count_map
                .entry(vertex.get_label())
                .or_insert(0) += 1;
        }
        for target_v_label in target_v_labels {
            if vertex_label_count_map
                .get(&target_v_label)
                .cloned()
                .unwrap_or(0)
                >= same_label_vertex_limit
            {
                continue;
            }
            // The collection of extend edges with a source vertex id
            // The source vertex id is used to specify the extend edge is from which vertex of the pattern
            let mut extend_edges_with_src_id = vec![];
            for (_, src_vertex) in &self.vertices {
                // check whether there are some edges between the target vertex and the current source vertex
                let adjacent_edges =
                    pattern_meta.associated_elabels_iter_by_vlabel(src_vertex.get_label(), target_v_label);
                // Transform all the adjacent edges to ExtendEdge and add to extend_edges_with_src_id
                for (adjacent_edge_label, adjacent_edge_dir) in adjacent_edges {
                    let extend_edge = ExtendEdge::new(
                        self.get_vertex_rank(src_vertex.get_id())
                            .unwrap(),
                        adjacent_edge_label,
                        adjacent_edge_dir,
                    );
                    extend_edges_with_src_id.push((extend_edge, src_vertex.get_id()));
                }
            }
            // Get the subsets of extend_edges_with_src_id, and add every subset to the extend_edges_set_collection
            // The algorithm is BFS Search
            let extend_edges_set_collection =
                get_subsets(extend_edges_with_src_id, |(_, src_id_for_check), extend_edges_set| {
                    limit_repeated_element_num(
                        src_id_for_check,
                        extend_edges_set.iter().map(|(_, v_id)| v_id),
                        1,
                    )
                });
            for extend_edges in extend_edges_set_collection {
                let extend_step = ExtendStep::new(
                    target_v_label,
                    extend_edges
                        .into_iter()
                        .map(|(extend_edge, _)| extend_edge)
                        .collect(),
                );
                extend_steps.push(extend_step);
            }
        }
        extend_steps
    }

    /// Edit the pattern by connect some edges to the current pattern
    fn add_edge(&mut self, edge: &PatternEdge) -> IrResult<()> {
        // Error that the adding edge already exist
        if self.edges.contains_key(edge.get_id()) {
            return Err(IrError::InvalidCode("The adding edge already existed".to_string()));
        }
        let start_vertex = edge.get_start_vertex();
        let end_vertex = edge.get_end_vertex();
        // Error that cannot connect the edge to the pattern
        if let (None, None) =
            (self.vertices.get(start_vertex.get_id()), self.vertices.get(end_vertex.get_id()))
        {
            return Err(IrError::InvalidCode("The adding edge cannot connect to the pattern".to_string()));
        } else if self
            .vertices
            .get(start_vertex.get_id())
            .is_none()
        {
            // end vertex already exists in the pattern, use it to connect
            // add start vertex
            self.vertices
                .insert(start_vertex.get_id(), start_vertex);
            self.vertices_data
                .insert(start_vertex.get_id(), PatternVertexData::default());
        } else if self.vertices.get(end_vertex.get_id()).is_none() {
            // start vertex already exists in the pattern, use it to connect
            // add end vertex
            self.vertices
                .insert(end_vertex.get_id(), end_vertex);
            self.vertices_data
                .insert(end_vertex.get_id(), PatternVertexData::default());
        }
        // update start vertex's connection info
        if let Some(start_vertex_data) = self
            .vertices_data
            .get_mut(start_vertex.get_id())
        {
            start_vertex_data
                .out_adjacencies
                .push(Adjacency::new(&start_vertex, edge).unwrap());
        }
        // update end vertex's connection info
        if let Some(end_vertex_data) = self.vertices_data.get_mut(end_vertex.get_id()) {
            end_vertex_data
                .in_adjacencies
                .push(Adjacency::new(&end_vertex, edge).unwrap());
        }
        // add edge to the pattern
        self.edges.insert(edge.get_id(), edge.clone());
        self.edges_data
            .insert(edge.get_id(), PatternEdgeData::default());
        Ok(())
    }

    /// Add a series of edges to the current pattern to get a new pattern
    pub fn extend_by_edges<'a, T>(&self, edges: T) -> IrResult<Pattern>
    where
        T: Iterator<Item = &'a PatternEdge>,
    {
        let mut new_pattern = self.clone();
        for edge in edges {
            new_pattern.add_edge(edge)?;
        }
        new_pattern.canonical_labeling();
        Ok(new_pattern)
    }

    pub fn extend_definitely(
        &self, extend_edge: &DefiniteExtendEdge, target_vetex: PatternVertex,
    ) -> Option<Pattern> {
        let pattern_edge = if let PatternDirection::Out = extend_edge.get_direction() {
            PatternEdge::new(
                extend_edge.get_edge_id(),
                extend_edge.get_edge_label(),
                extend_edge.get_src_vertex(),
                target_vetex,
            )
        } else {
            PatternEdge::new(
                extend_edge.get_edge_id(),
                extend_edge.get_edge_label(),
                target_vetex,
                extend_edge.get_src_vertex(),
            )
        };
        let mut new_pattern = self.clone();
        if new_pattern.add_edge(&pattern_edge).is_ok() {
            new_pattern.canonical_labeling();
            Some(new_pattern)
        } else {
            None
        }
    }

    /// Locate a vertex(id) from the pattern based on the given extend step and target pattern code
    pub fn locate_vertex(
        &self, extend_step: &ExtendStep, target_pattern_code: &Vec<u8>,
    ) -> Option<PatternId> {
        let mut target_vertex_id: Option<PatternId> = None;
        let target_v_label = extend_step.get_target_vertex_label();
        // mark all the vertices with the same label as the extend step's target vertex as the candidates
        for target_v_cand in self.vertices_iter_by_label(target_v_label) {
            if self.get_vertex_degree(target_v_cand.get_id()) != extend_step.get_extend_edges_num() {
                continue;
            }
            // compare whether the candidate vertex has the same connection info as the extend step
            let cand_e_label_dir_set: BTreeSet<(PatternLabelId, PatternDirection)> = self
                .adjacencies_iter(target_v_cand.get_id())
                .map(|adjacency| (adjacency.get_edge_label(), adjacency.get_direction().reverse()))
                .collect();
            let extend_e_label_dir_set: BTreeSet<(PatternLabelId, PatternDirection)> = extend_step
                .iter()
                .map(|extend_edge| (extend_edge.get_edge_label(), extend_edge.get_direction()))
                .collect();
            // if has the same connection info, check whether the pattern after the removing the target vertex
            // has the same code with the target pattern code
            if cand_e_label_dir_set == extend_e_label_dir_set {
                let check_pattern = self
                    .clone()
                    .remove_vertex(target_v_cand.get_id())
                    .unwrap();
                let check_pattern_code = check_pattern.encode_to();
                // same code means successfully locate the vertex
                if check_pattern_code == *target_pattern_code {
                    target_vertex_id = Some(target_v_cand.get_id());
                    break;
                }
            }
        }
        target_vertex_id
    }

    /// Remove a vertex with all its adjacent edges in the current pattern
    pub fn remove_vertex(mut self, vertex_id: PatternId) -> Option<Pattern> {
        if self.get_vertex(vertex_id).is_some() {
            let adjacencies: Vec<Adjacency> = self
                .adjacencies_iter(vertex_id)
                .cloned()
                .collect();
            self.remove_vertex_internal(vertex_id);
            for adjacency in adjacencies {
                let adjacent_vertex_id = adjacency.get_adj_vertex().get_id();
                let adjacent_edge_id = adjacency.get_edge_id();
                // delete adjacent edges
                // delete in edges
                self.edges.remove(adjacent_edge_id);
                // delete in edge tag map
                if let Some(tag) = self.get_edge_tag(adjacent_edge_id) {
                    self.tag_edge_map.remove(&tag);
                }
                // delete in edges data
                self.edges_data.remove(adjacent_edge_id);
                // update adjcent vertices's info
                if let PatternDirection::Out = adjacency.get_direction() {
                    self.vertices_data
                        .get_mut(adjacent_vertex_id)
                        .unwrap()
                        .in_adjacencies
                        .retain(|adj| adj.get_edge_id() != adjacent_edge_id)
                } else {
                    self.vertices_data
                        .get_mut(adjacent_vertex_id)
                        .unwrap()
                        .out_adjacencies
                        .retain(|adj| adj.get_edge_id() != adjacent_edge_id)
                }
            }
            self.canonical_labeling();
            if self.is_connected() {
                Some(self)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Remove a vertex with all its adjacent edges in the current pattern
    pub fn remove_vertex_local(&mut self, vertex_id: PatternId) {
        if self.get_vertex(vertex_id).is_some() {
            let adjacencies: Vec<Adjacency> = self
                .adjacencies_iter(vertex_id)
                .cloned()
                .collect();
            // delete target vertex
            // delete in vertices
            self.vertices.remove(vertex_id);
            // delete in vertex tag map
            if let Some(tag) = self.get_vertex_tag(vertex_id) {
                self.tag_vertex_map.remove(&tag);
            }
            // delete in vertices data
            self.vertices_data.remove(vertex_id);
            for adjacency in adjacencies {
                let adjacent_vertex_id = adjacency.get_adj_vertex().get_id();
                let adjacent_edge_id = adjacency.get_edge_id();
                // delete adjacent edges
                // delete in edges
                self.edges.remove(adjacent_edge_id);
                // delete in edge tag map
                if let Some(tag) = self.get_edge_tag(adjacent_edge_id) {
                    self.tag_edge_map.remove(&tag);
                }
                // delete in edges data
                self.edges_data.remove(adjacent_edge_id);
                // update adjcent vertices's info
                if let PatternDirection::Out = adjacency.get_direction() {
                    self.vertices_data
                        .get_mut(adjacent_vertex_id)
                        .unwrap()
                        .in_adjacencies
                        .retain(|adj| adj.get_edge_id() != adjacent_edge_id)
                } else {
                    self.vertices_data
                        .get_mut(adjacent_vertex_id)
                        .unwrap()
                        .out_adjacencies
                        .retain(|adj| adj.get_edge_id() != adjacent_edge_id)
                }
            }

            self.canonical_labeling();
        }
    }

    /// Delete a extend step from current pattern to get a new pattern
    ///
    /// The code of the new pattern should be the same as the target pattern code
    pub fn de_extend(&self, extend_step: &ExtendStep, target_pattern_code: &Vec<u8>) -> Option<Pattern> {
        if let Some(target_vertex_id) = self.locate_vertex(extend_step, target_pattern_code) {
            self.clone().remove_vertex(target_vertex_id)
        } else {
            None
        }
    }

    pub fn remove_edge(mut self, edge_id: PatternId) -> Option<Pattern> {
        if let Some(edge) = self.get_edge(edge_id).cloned() {
            self.remove_edge_internal(edge_id);
            let start_vertex = edge.get_start_vertex().get_id();
            let end_vertex = edge.get_end_vertex().get_id();
            // update start vertex's info
            self.vertices_data
                .get_mut(start_vertex)
                .unwrap()
                .out_adjacencies
                .retain(|adj| adj.get_edge_id() != edge_id);
            if self.get_vertex_degree(start_vertex) == 0 && self.get_vertices_num() > 1 {
                self.remove_vertex_internal(start_vertex)
            }
            // update end vertex's info
            self.vertices_data
                .get_mut(end_vertex)
                .unwrap()
                .in_adjacencies
                .retain(|adj| adj.get_edge_id() != edge_id);
            if self.get_vertex_degree(end_vertex) == 0 && self.get_vertices_num() > 1 {
                self.remove_vertex_internal(end_vertex)
            }
            if self.is_connected() {
                self.canonical_labeling();
                Some(self)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn remove_vertex_internal(&mut self, vertex_id: PatternId) {
        // delete vertex
        // delete in vertices
        self.vertices.remove(vertex_id);
        // delete in vertex tag map
        if let Some(tag) = self.get_vertex_tag(vertex_id) {
            self.tag_vertex_map.remove(&tag);
        }
        // delete in vertices data
        self.vertices_data.remove(vertex_id);
    }

    fn remove_edge_internal(&mut self, edge_id: PatternId) {
        // delete edge
        // delete in edges
        self.edges.remove(edge_id);
        // delete in edge tag map
        if let Some(tag) = self.get_edge_tag(edge_id) {
            self.tag_edge_map.remove(&tag);
        }
        // delete in edges data
        self.edges_data.remove(edge_id);
    }

    // fn is_connected(&self) -> bool {
    //     let mut visted_vertices = HashSet::new();
    //     let start_vertex = self.vertices_iter().next().unwrap().get_id();
    //     let mut stack = vec![start_vertex];
    //     while let Some(src_vertex) = stack.pop() {
    //         visted_vertices.insert(src_vertex);
    //         for neighbor_vertex in self
    //             .adjacencies_iter(src_vertex)
    //             .map(|adj| adj.get_adj_vertex().get_id())
    //             .filter(|vertex| !visted_vertices.contains(&vertex))
    //         {
    //             stack.push(neighbor_vertex);
    //         }
    //     }
    //     visted_vertices.len() == self.get_vertices_num()
    // }
}

impl Serialize for Pattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let pattern_code = self.encode_to();
        serializer.serialize_bytes(&pattern_code)
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct PatternVisitor;
        impl<'de> Visitor<'de> for PatternVisitor {
            type Value = Pattern;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("Read Pattern")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Pattern::decode_from(v).ok_or(E::custom("Invalide Pattern Code"))
            }
        }
        deserializer.deserialize_bytes(PatternVisitor)
    }
}

#[derive(Serialize, Deserialize)]
struct GCardPatternVertex {
    tag_id: u8,
    label_id: u32,
}

#[derive(Serialize, Deserialize)]
struct GCardPatternEdge {
    tag_id: u8,
    src: u8,
    dst: u8,
    label_id: u32,
}

#[derive(Serialize, Deserialize)]
pub struct PatternWithCount {
    vertices: Vec<GCardPatternVertex>,
    edges: Vec<GCardPatternEdge>,
    count: Option<OrderedFloat<f64>>,
}

impl From<Pattern> for PatternWithCount {
    fn from(value: Pattern) -> Self {
        let vertices = value
            .vertices
            .values()
            .map(|v| GCardPatternVertex { tag_id: v.get_id() as _, label_id: v.get_label() as _ })
            .collect();
        let edges = value
            .edges
            .values()
            .map(|e| GCardPatternEdge {
                tag_id: e.get_id() as _,
                src: e.get_start_vertex().get_id() as _,
                dst: e.get_end_vertex().get_id() as _,
                label_id: e.get_label() as _,
            })
            .collect();
        let count = None;
        Self { vertices, edges, count }
    }
}

impl TryFrom<PatternWithCount> for Pattern {
    type Error = IrError;

    fn try_from(value: PatternWithCount) -> IrResult<Self> {
        let PatternWithCount { vertices, edges, count: _ } = value;
        if edges.is_empty() {
            if vertices.len() != 1 {
                return Err(IrError::InvalidPattern(
                    "Pattern without edges has more than one vertex".into(),
                ));
            }
            let GCardPatternVertex { tag_id, label_id } = vertices.into_iter().next().unwrap();
            let vertex = PatternVertex::new(tag_id as _, label_id as _);
            Ok(Pattern::from(vertex))
        } else {
            let vertex_map: HashMap<_, _> = vertices
                .into_iter()
                .map(|v| (v.tag_id, PatternVertex::new(v.tag_id as _, v.label_id as _)))
                .collect();
            let pattern_edges: Vec<_> = edges
                .into_iter()
                .map(|GCardPatternEdge { tag_id, src, dst, label_id }| -> IrResult<_> {
                    let start = vertex_map.get(&src).copied().ok_or_else(|| {
                        IrError::InvalidPattern(format!("start vertex {src} does not exist."))
                    })?;
                    let end = vertex_map.get(&dst).copied().ok_or_else(|| {
                        IrError::InvalidPattern(format!("end vertex {dst} does not exist."))
                    })?;
                    Ok(PatternEdge::new(tag_id as _, label_id as _, start, end))
                })
                .try_collect()?;
            Pattern::try_from(pattern_edges)
        }
    }
}

impl PatternWithCount {
    pub fn count(&self) -> Option<OrderedFloat<f64>> {
        self.count
    }

    pub fn export<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        Ok(serde_json::to_writer_pretty(writer, self)?)
    }

    pub fn import<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        Ok(serde_json::from_reader(reader)?)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, iter::FromIterator};

    use super::*;

    #[test]
    fn test_join_id_label_maps() {
        let left_maps = vec![BTreeMap::from_iter([(5, 1), (6, 2)]), BTreeMap::from_iter([(5, 3), (6, 4)])];
        let right_maps = vec![
            BTreeMap::from_iter([(6, 1), (7, 1)]),
            BTreeMap::from_iter([(6, 2), (7, 2)]),
            BTreeMap::from_iter([(6, 2), (7, 3)]),
            BTreeMap::from_iter([(6, 4), (7, 4)]),
        ];
        let joined_maps = join_id_label_maps(left_maps, right_maps, &mut false);
        assert_eq!(joined_maps.len(), 3);
    }
}
