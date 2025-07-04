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
//!

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::TryFrom;
use std::fs::File;
use std::iter::FromIterator;
use std::path::Path;
use std::sync::{mpsc, mpsc::Sender, Arc};
use std::{thread, thread::JoinHandle, vec};

use graph_store::config::{DIR_GRAPH_SCHEMA, FILE_SCHEMA};
use graph_store::prelude::{DefaultId, GlobalStoreTrait, GraphDBConfig, InternalId, LabelId, LargeGraphDB};
use log::info;
use petgraph::graph::NodeIndex;

use crate::catalogue::catalog::{Catalogue, TableLogue};
use crate::catalogue::extend_step::{DefiniteExtendEdge, DefiniteExtendStep, ExtendStep};
use crate::catalogue::pattern::Pattern;
use crate::catalogue::pattern_meta::PatternMeta;
use crate::catalogue::plan::get_definite_extend_steps;
use crate::catalogue::{DynIter, PatternId, PatternLabelId};
use crate::plan::meta::Schema;
use crate::JsonIO;

type PatternRecord = BTreeMap<PatternId, DefaultId>;

impl Catalogue {
    pub fn estimate_graph(
        &mut self, graph: Arc<LargeGraphDB<DefaultId, InternalId>>, rate: f64,
        sparsify_rate: HashMap<(u8, u8, u8), f64>, limit: Option<usize>, thread_num: usize,
    ) {
        // Store the count of patterns
        let mut pattern_counts_map = HashMap::new();
        // The start points of the overal estimate graph process
        let mut pattern_count_infos = self.get_start_pattern_count_infos(&graph, rate, limit);
        // Store start patterns' count
        update_pattern_counts_map(&mut pattern_counts_map, &pattern_count_infos);
        // Count patterns in the catalog level by level
        while !pattern_count_infos.is_empty() {
            // Generate sub tasks to get of count infos of next level's pattern
            let sub_tasks = self.generate_sub_tasks(pattern_count_infos, &graph);
            // Execute Subtasks
            pattern_count_infos = self.execcute_sub_tasks(sub_tasks, thread_num, rate, limit);
            // Store patterns' count
            update_pattern_counts_map(&mut pattern_counts_map, &pattern_count_infos);
        }
        info!("{:?}", pattern_counts_map);
        // Set pattern count in the catalog with sparsify rate info
        for (&pattern_index, &pattern_count) in pattern_counts_map.iter() {
            self.set_pattern_count_with_rate(pattern_index, pattern_count, &sparsify_rate);
        }
        // Set extend count in the catalog
        for (&pattern_index, _) in pattern_counts_map.iter() {
            self.set_extend_count_infos(pattern_index)
        }
    }

    fn get_start_pattern_indices(&self) -> Vec<NodeIndex> {
        self.entries_iter().collect()
    }

    fn get_start_pattern_count_infos(
        &mut self, graph: &LargeGraphDB<DefaultId, InternalId>, rate: f64, limit: Option<usize>,
    ) -> HashMap<NodeIndex, Arc<PatternCountInfo>> {
        let mut pattern_nodes = HashMap::new();
        for start_pattern_index in self.get_start_pattern_indices() {
            let pattern = self
                .get_pattern_weight(start_pattern_index)
                .unwrap()
                .get_pattern()
                .clone();
            let (extend_steps, _) = get_definite_extend_steps(pattern.clone(), self);
            let mut pattern_records = get_src_records(graph, extend_steps, limit);
            let pattern_count = pattern_records.len();
            pattern_records = sample_records(pattern_records, rate, limit);
            pattern_nodes.insert(
                start_pattern_index,
                Arc::new(PatternCountInfo::new(pattern, pattern_records, pattern_count)),
            );
        }
        pattern_nodes
    }

    fn generate_sub_tasks(
        &self, pattern_count_infos: HashMap<NodeIndex, Arc<PatternCountInfo>>,
        graph: &Arc<LargeGraphDB<DefaultId, InternalId>>,
    ) -> HashMap<NodeIndex, SubTask> {
        let next_pattern_indices: HashSet<NodeIndex> = pattern_count_infos
            .iter()
            .flat_map(|(&pattern_index, _)| self.pattern_out_approaches_iter(pattern_index))
            .filter(|approach| {
                self.get_approach_weight(approach.get_approach_index())
                    .and_then(|approach_weight| approach_weight.get_extend_weight())
                    .is_some()
            })
            .map(|approach| approach.get_target_pattern_index())
            .collect();
        let mut sub_tasks = HashMap::new();
        for next_pattern_index in next_pattern_indices {
            let mut min_count = usize::MAX;
            let mut pre_pattern_count_min = None;
            let mut extend_step = None;
            for approach in self.pattern_in_approaches_iter(next_pattern_index) {
                if let Some(pre_pattern_count) = pattern_count_infos.get(&approach.get_src_pattern_index())
                {
                    if pre_pattern_count.pattern_count < min_count
                        && !pre_pattern_count.pattern_records.is_empty()
                    {
                        min_count = pre_pattern_count.pattern_count;
                        pre_pattern_count_min = Some(pre_pattern_count.clone());
                        extend_step = Some(Arc::new(
                            self.get_extend_weight(approach.get_approach_index())
                                .unwrap()
                                .get_extend_step()
                                .clone(),
                        ))
                    }
                }
            }
            if let Some(count) = pre_pattern_count_min {
                sub_tasks.insert(next_pattern_index, SubTask::new(&count, &extend_step.unwrap(), graph));
            }
        }
        sub_tasks
    }

    fn execcute_sub_tasks(
        &self, sub_tasks: HashMap<NodeIndex, SubTask>, thread_num: usize, rate: f64, limit: Option<usize>,
    ) -> HashMap<NodeIndex, Arc<PatternCountInfo>> {
        let mut next_pattern_count_infos = HashMap::new();
        for (target_pattern_index, sub_task) in sub_tasks {
            let is_end = self
                .pattern_out_approaches_iter(target_pattern_index)
                .next()
                .is_none();
            let sub_task_result = sub_task.execute(thread_num, rate, limit, is_end);
            let target_pattern = sub_task
                .pattern_count_info
                .pattern
                .extend(&sub_task.extend_step)
                .unwrap();
            next_pattern_count_infos.insert(
                target_pattern_index,
                Arc::new(PatternCountInfo::new(
                    target_pattern,
                    sub_task_result.target_pattern_records,
                    sub_task_result.target_pattern_count,
                )),
            );
        }
        next_pattern_count_infos
    }

    fn set_pattern_count_with_rate(
        &mut self, pattern_index: NodeIndex, pattern_count: usize,
        sparsify_rate: &HashMap<(u8, u8, u8), f64>,
    ) {
        let pattern = self
            .get_pattern_weight(pattern_index)
            .unwrap()
            .get_pattern();
        let mut estimate_result = pattern_count as f64;
        for edge in pattern.edges_iter() {
            let src = edge.get_start_vertex().get_label();
            let edge_label = edge.get_label();
            let dst = edge.get_end_vertex().get_label();
            let keys = (src as u8, edge_label as u8, dst as u8);
            if sparsify_rate.contains_key(&keys) {
                estimate_result /= sparsify_rate[&keys];
            }
        }
        self.set_pattern_count_with_index(pattern_index, estimate_result.into())
    }
}

fn update_pattern_counts_map(
    pattern_counts_map: &mut HashMap<NodeIndex, usize>,
    pattern_count_infos: &HashMap<NodeIndex, Arc<PatternCountInfo>>,
) {
    for (&pattern_index, pattern_count_info) in pattern_count_infos.iter() {
        pattern_counts_map.insert(pattern_index, pattern_count_info.pattern_count);
    }
}

impl TableLogue {
    pub fn estimate_graph(
        &mut self, graph: Arc<LargeGraphDB<DefaultId, InternalId>>, rate: f64, limit: Option<usize>,
        thread_num: usize,
    ) {
        let mut start_patterns_codes = HashSet::new();
        let mut src_patterns = HashSet::new();
        for pattern in self.iter().map(|row| row.get_src_pattern()) {
            if pattern.get_vertices_num() == 1 {
                start_patterns_codes.insert(pattern.encode_to());
            }
            src_patterns.insert(pattern.encode_to());
        }
        let mut pattern_count_infos = HashMap::new();
        for pattern_code in start_patterns_codes.iter() {
            let pattern = Pattern::decode_from(pattern_code).unwrap();
            let extend_step = DefiniteExtendStep::try_from(pattern.clone()).unwrap();
            let mut pattern_records = get_src_records(&graph, vec![extend_step], limit);
            let pattern_count = pattern_records.len();
            pattern_records = sample_records(pattern_records, rate, limit);
            pattern_count_infos.insert(
                pattern_code.clone(),
                Arc::new(PatternCountInfo::new(pattern, pattern_records, pattern_count)),
            );
        }
        for row in self.iter_mut() {
            let src_pattern = row.get_src_pattern();
            let src_pattern_code = src_pattern.encode_to();
            let src_pattern_count_infos = pattern_count_infos
                .get(&src_pattern_code)
                .unwrap();
            let extend_step = Arc::new(row.get_extend_step().clone());
            let sub_task = SubTask::new(src_pattern_count_infos, &extend_step, &graph);
            let sub_task_result = sub_task.execute(thread_num, rate, limit, false);
            let target_pattern = src_pattern.extend(&extend_step).unwrap();
            let target_pattern_code = target_pattern.encode_to();
            if !pattern_count_infos.contains_key(&target_pattern_code)
                && src_patterns.contains(&target_pattern_code)
            {
                pattern_count_infos.insert(
                    target_pattern_code,
                    Arc::new(PatternCountInfo::new(
                        target_pattern,
                        sub_task_result.target_pattern_records,
                        sub_task_result.target_pattern_count,
                    )),
                );
            }
            row.set_pattern_count(sub_task_result.target_pattern_count);
        }
    }
}

#[derive(Debug, Clone)]
struct PatternCountInfo {
    pattern: Pattern,
    pattern_records: Vec<PatternRecord>,
    pattern_count: usize,
}

impl PatternCountInfo {
    fn new(
        pattern: Pattern, pattern_records: Vec<PatternRecord>, pattern_count: usize,
    ) -> PatternCountInfo {
        PatternCountInfo { pattern, pattern_records, pattern_count }
    }
}

#[derive(Clone)]
struct SubTask {
    pattern_count_info: Arc<PatternCountInfo>,
    extend_step: Arc<ExtendStep>,
    graph: Arc<LargeGraphDB<DefaultId, InternalId>>,
}

impl SubTask {
    fn new(
        pattern_count_info: &Arc<PatternCountInfo>, extend_step: &Arc<ExtendStep>,
        graph: &Arc<LargeGraphDB<DefaultId, InternalId>>,
    ) -> SubTask {
        SubTask {
            pattern_count_info: Arc::clone(pattern_count_info),
            extend_step: Arc::clone(extend_step),
            graph: Arc::clone(graph),
        }
    }

    fn get_pattern(&self) -> &Pattern {
        &self.pattern_count_info.pattern
    }

    fn get_pattern_records(&self) -> &Vec<PatternRecord> {
        &self.pattern_count_info.pattern_records
    }

    fn get_pattern_count(&self) -> usize {
        self.pattern_count_info.pattern_count
    }
}

impl SubTask {
    fn execute(&self, thread_num: usize, rate: f64, limit: Option<usize>, is_end: bool) -> SubTaskResult {
        debug!("execute subtask: {}", self.get_pattern());
        let mut target_pattern_count = 0;
        let mut target_pattern_records = Vec::new();
        let (tx_record_count, rx_record_count) = mpsc::channel();
        let (tx_records, rx_records) = mpsc::channel();
        let mut thread_handles = Vec::with_capacity(thread_num);
        for thread_id in 0..thread_num {
            let thread_sub_task = self.clone();
            let thread_handle = thread_sub_task.execute_internal(
                thread_id,
                thread_num,
                tx_record_count.clone(),
                tx_records.clone(),
                is_end,
            );
            thread_handles.push(thread_handle);
        }
        for thread_handle in thread_handles {
            thread_handle.join().unwrap();
        }
        while let Ok(partial_count) = rx_record_count.try_recv() {
            target_pattern_count += partial_count;
        }
        while let Ok(target_pattern_record) = rx_records.try_recv() {
            target_pattern_records.push(target_pattern_record);
        }
        let target_pattern_count = if self.get_pattern_records().is_empty() {
            0
        } else {
            (self.get_pattern_count() as f64
                * (target_pattern_count as f64 / self.get_pattern_records().len() as f64))
                as usize
        };
        SubTaskResult::new(sample_records(target_pattern_records, rate, limit), target_pattern_count)
    }

    fn execute_internal(
        self, thread_id: usize, thread_num: usize, tx_record_count: Sender<usize>,
        tx_records: Sender<PatternRecord>, is_end: bool,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            let target_vertex_id = self.get_pattern().get_max_vertex_id() + 1;
            let mut target_pattern_partial_count = 0;
            for pattern_record in split_vector(self.get_pattern_records(), thread_num, thread_id) {
                let mut intersect_vertices_set = BTreeSet::new();
                for (i, extend_edge) in self.extend_step.iter().enumerate() {
                    let adj_vertices_set = get_adj_vertices_set(
                        &self.graph,
                        pattern_record,
                        &DefiniteExtendEdge::from_extend_edge(extend_edge, self.get_pattern()).unwrap(),
                        self.extend_step.get_target_vertex_label(),
                    );
                    intersect_vertices_set =
                        intersect_sets(intersect_vertices_set, adj_vertices_set, i == 0);
                }
                for target_pattern_record in intersect_vertices_set
                    .iter()
                    .map(|&adj_vertex_id| {
                        let mut target_pattern_record = pattern_record.clone();
                        target_pattern_record.insert(target_vertex_id, adj_vertex_id);
                        target_pattern_record
                    })
                {
                    if !is_end {
                        tx_records.send(target_pattern_record).unwrap();
                    }
                }
                target_pattern_partial_count += intersect_vertices_set.len();
            }
            tx_record_count
                .send(target_pattern_partial_count)
                .unwrap();
        })
    }
}

struct SubTaskResult {
    target_pattern_records: Vec<PatternRecord>,
    target_pattern_count: usize,
}

impl SubTaskResult {
    fn new(target_pattern_records: Vec<PatternRecord>, target_pattern_count: usize) -> SubTaskResult {
        SubTaskResult { target_pattern_records, target_pattern_count }
    }
}

pub fn get_src_records(
    graph: &LargeGraphDB<DefaultId, InternalId>, extend_steps: Vec<DefiniteExtendStep>,
    limit: Option<usize>,
) -> Vec<PatternRecord> {
    let mut extend_steps = extend_steps.into_iter();
    let first_extend_step = extend_steps.next().unwrap();
    let src_vertex = first_extend_step.get_target_vertex();
    let src_vertex_label = src_vertex.get_label();
    let src_pattern_vertex_id = src_vertex.get_id();
    let mut pattern_records: DynIter<PatternRecord> = Box::new(
        graph
            .get_all_vertices(Some(&vec![src_vertex_label as LabelId]))
            .map(|graph_vertex| PatternRecord::from_iter([(src_pattern_vertex_id, graph_vertex.get_id())])),
    );
    for extend_step in extend_steps {
        if let Some(upper_bound) = limit {
            pattern_records = Box::new(pattern_records.take(upper_bound));
        }
        pattern_records = Box::new(pattern_records.flat_map(move |pattern_record| {
            let target_vertex = extend_step.get_target_vertex();
            let target_vertex_label = target_vertex.get_label();
            let mut intersect_vertices = BTreeSet::new();
            for (i, extend_edge) in extend_step.iter().enumerate() {
                let adjacent_vertices =
                    get_adj_vertices_set(graph, &pattern_record, extend_edge, target_vertex_label);
                intersect_vertices = intersect_sets(intersect_vertices, adjacent_vertices, i == 0);
            }
            let target_pattern_vertex_id = target_vertex.get_id();
            intersect_vertices
                .into_iter()
                .map(move |adj_graph_vertex_id| {
                    let mut new_pattern_record = pattern_record.clone();
                    new_pattern_record.insert(target_pattern_vertex_id, adj_graph_vertex_id);
                    new_pattern_record
                })
        }));
    }
    pattern_records.collect()
}

fn get_adj_vertices_set(
    graph: &LargeGraphDB<DefaultId, InternalId>, pattern_record: &PatternRecord,
    extend_edge: &DefiniteExtendEdge, target_vertex_label: PatternLabelId,
) -> BTreeSet<DefaultId> {
    let src_pattern_vertex_id = extend_edge.get_src_vertex().get_id();
    let src_graph_vertex_id = *pattern_record
        .get(&src_pattern_vertex_id)
        .unwrap();
    let edge_label = extend_edge.get_edge_label();
    let direction = extend_edge.get_direction();
    graph
        .get_adj_vertices(src_graph_vertex_id, Some(&vec![edge_label as LabelId]), direction.into())
        .filter(|graph_vertex| graph_vertex.get_label()[0] == (target_vertex_label as LabelId))
        .map(|graph_vertex| graph_vertex.get_id())
        .collect()
}

fn intersect_sets<T: Clone + Ord>(set1: BTreeSet<T>, set2: BTreeSet<T>, is_start: bool) -> BTreeSet<T> {
    if is_start {
        set2
    } else {
        set1.intersection(&set2).cloned().collect()
    }
}

fn sample_records(mut records: Vec<PatternRecord>, rate: f64, limit: Option<usize>) -> Vec<PatternRecord> {
    if let Some(lower_bound) = limit {
        if records.len() <= lower_bound {
            return records;
        }
    }
    let expected_len = if let Some(upper_bound) = limit {
        std::cmp::min(((records.len() as f64) * rate).floor() as usize, upper_bound)
    } else {
        ((records.len() as f64) * rate).floor() as usize
    };
    let step = (records.len() as f64 / expected_len as f64).floor() as usize;
    if step > 1 {
        let picked_indices = (0..(records.len() / step)).map(|i| i * step);
        for (i, pi) in picked_indices.enumerate() {
            records.swap(i, pi);
        }
    }
    records.truncate(expected_len);
    records
}

fn split_vector<T>(vector: &[T], thread_num: usize, thread_id: usize) -> &[T] {
    let start_index = (vector.len() / thread_num) * thread_id;
    let end_index = if thread_id == thread_num - 1 {
        vector.len()
    } else {
        (vector.len() / thread_num) * (thread_id + 1)
    };
    &vector[start_index..end_index]
}

pub fn load_sample_graph(graph_path: &str) -> LargeGraphDB<DefaultId, InternalId> {
    info!("Read the sample graph data from {:?}.", graph_path);
    GraphDBConfig::default()
        .root_dir(graph_path)
        .partition(1)
        .schema_file(
            Path::new(graph_path)
                .join(DIR_GRAPH_SCHEMA)
                .join(FILE_SCHEMA),
        )
        .open()
        .expect("Open graph error")
}

pub fn load_pattern_meta(schema_path: &str) -> PatternMeta {
    info!("Read the patternm meta from {:?}.", schema_path);
    let schema_file = File::open(schema_path).unwrap();
    let schema = Schema::from_json(schema_file).unwrap();
    PatternMeta::from(schema)
}

// #[cfg(test)]
// mod tests {
//     use std::convert::TryFrom;

//     use crate::catalogue::extend_step::{ExtendEdge, ExtendStep};
//     use crate::catalogue::pattern::{PatternEdge, PatternVertex};
//     use crate::catalogue::plan::get_definite_extend_steps_recursively;
//     use crate::catalogue::sample::*;
//     use crate::catalogue::PatternDirection;

//     fn get_src_records_from_label(
//         graph: &LargeGraphDB<DefaultId, InternalId>, vertex_label: PatternLabelId,
//     ) -> Vec<PatternRecord> {
//         graph
//             .get_all_vertices(Some(&vec![vertex_label as LabelId]))
//             .map(|graph_vertex| PatternRecord::from_iter([(0, graph_vertex.get_id())]))
//             .collect()
//     }

//     fn get_adj_vertices_set(
//         graph: &LargeGraphDB<DefaultId, InternalId>, src_pattern: &Pattern, extend_edge: &ExtendEdge,
//         target_vertex_label: PatternLabelId, pattern_record: &PatternRecord,
//     ) -> Option<BTreeSet<DefaultId>> {
//         if let Some(src_pattern_vertex) =
//             src_pattern.get_vertex_from_rank(extend_edge.get_src_vertex_rank())
//         {
//             let src_pattern_vertex_id = src_pattern_vertex.get_id();
//             if let Some(&src_graph_vertex_id) = pattern_record.get(&src_pattern_vertex_id) {
//                 let direction = extend_edge.get_direction();
//                 let edge_label = extend_edge.get_edge_label();
//                 Some(
//                     graph
//                         .get_adj_vertices(
//                             src_graph_vertex_id,
//                             Some(&vec![edge_label as LabelId]),
//                             direction.into(),
//                         )
//                         .filter(|graph_vertex| {
//                             graph_vertex.get_label()[0] == (target_vertex_label as LabelId)
//                         })
//                         .map(|graph_vertex| graph_vertex.get_id())
//                         .collect(),
//                 )
//             } else {
//                 None
//             }
//         } else {
//             None
//         }
//     }

//     fn get_adj_vertices_sets(
//         graph: &LargeGraphDB<DefaultId, InternalId>, src_pattern: &Pattern, extend_step: &ExtendStep,
//         pattern_record: &PatternRecord,
//     ) -> Option<Vec<(ExtendEdge, BTreeSet<DefaultId>)>> {
//         let mut adj_vertices_sets = vec![];
//         for extend_edge in extend_step.iter() {
//             if let Some(adj_vertices_set) = get_adj_vertices_set(
//                 graph,
//                 src_pattern,
//                 extend_edge,
//                 extend_step.get_target_vertex_label(),
//                 pattern_record,
//             ) {
//                 adj_vertices_sets.push((extend_edge.clone(), adj_vertices_set));
//             } else {
//                 return None;
//             }
//         }
//         Some(adj_vertices_sets)
//     }

//     fn intersect_adj_vertices_sets(
//         mut adj_vertices_sets: Vec<(ExtendEdge, BTreeSet<DefaultId>)>,
//     ) -> BTreeSet<DefaultId> {
//         adj_vertices_sets.sort_by(|(_, vertices_set1), (_, vertices_set2)| {
//             vertices_set1.len().cmp(&vertices_set2.len())
//         });
//         let (_, mut set_after_intersect) = adj_vertices_sets.pop().unwrap();
//         for (_, adj_vertices_set) in adj_vertices_sets.into_iter() {
//             set_after_intersect = set_after_intersect
//                 .intersection(&adj_vertices_set)
//                 .cloned()
//                 .collect();
//         }
//         set_after_intersect
//     }

//     #[test]
//     fn test_create_sample_graph() {
//         let sample_graph = load_sample_graph("../core/resource/test_graph");
//         let total_count = sample_graph.count_all_vertices(None);
//         let coach_count = sample_graph
//             .get_all_vertices(Some(&vec![0]))
//             .map(|vertex| vertex.get_id())
//             .collect::<Vec<DefaultId>>()
//             .len();
//         let player_count = sample_graph
//             .get_all_vertices(Some(&vec![1]))
//             .map(|vertex| vertex.get_id())
//             .collect::<Vec<DefaultId>>()
//             .len();
//         let fan_count = sample_graph
//             .get_all_vertices(Some(&vec![2]))
//             .map(|vertex| vertex.get_id())
//             .collect::<Vec<DefaultId>>()
//             .len();
//         let ticket_count = sample_graph
//             .get_all_vertices(Some(&vec![3]))
//             .map(|vertex| vertex.get_id())
//             .collect::<Vec<DefaultId>>()
//             .len();
//         assert_eq!(total_count, 30100);
//         assert_eq!(coach_count, 10000);
//         assert_eq!(player_count, 10000);
//         assert_eq!(fan_count, 10000);
//         assert_eq!(ticket_count, 100);
//     }

//     #[test]
//     fn test_get_src_records_from_label() {
//         let sample_graph = load_sample_graph("../core/resource/test_graph");
//         let coach_src_records = get_src_records_from_label(&sample_graph, 0);
//         assert_eq!(coach_src_records.len(), 10000);
//         let player_src_records = get_src_records_from_label(&sample_graph, 1);
//         assert_eq!(player_src_records.len(), 10000);
//         let fan_src_records = get_src_records_from_label(&sample_graph, 2);
//         assert_eq!(fan_src_records.len(), 10000);
//         let ticket_src_records = get_src_records_from_label(&sample_graph, 3);
//         assert_eq!(ticket_src_records.len(), 100);
//     }

//     #[test]
//     fn test_get_adj_vertices_set() {
//         let sample_graph = load_sample_graph("../core/resource/test_graph");
//         let coach_src_record = get_src_records_from_label(&sample_graph, 0)[0].clone();
//         let coach_src_pattern = Pattern::from(PatternVertex::new(0, 0));
//         let player_src_record = get_src_records_from_label(&sample_graph, 1)[0].clone();
//         let player_src_pattern = Pattern::from(PatternVertex::new(0, 1));
//         let fan_src_record = get_src_records_from_label(&sample_graph, 2)[0].clone();
//         let fan_src_pattern = Pattern::from(PatternVertex::new(0, 2));
//         let ticket_src_record = get_src_records_from_label(&sample_graph, 3)[0].clone();
//         let ticket_src_pattern = Pattern::from(PatternVertex::new(0, 3));
//         let guide_out_extend_edge = ExtendEdge::new(0, 0, PatternDirection::Out);
//         let guide_in_extend_edge = ExtendEdge::new(0, 0, PatternDirection::In);
//         let loved_by_out_extend_edge = ExtendEdge::new(0, 1, PatternDirection::Out);
//         let loved_by_in_extend_edge = ExtendEdge::new(0, 1, PatternDirection::In);
//         let buy_out_extend_edge = ExtendEdge::new(0, 2, PatternDirection::Out);
//         let buy_in_extend_edge = ExtendEdge::new(0, 2, PatternDirection::In);
//         let players_from_coach_guide = get_adj_vertices_set(
//             &sample_graph,
//             &coach_src_pattern,
//             &guide_out_extend_edge,
//             1,
//             &coach_src_record,
//         )
//         .unwrap();
//         assert_eq!(players_from_coach_guide.len(), 100);
//         let coaches_from_player_guide = get_adj_vertices_set(
//             &sample_graph,
//             &player_src_pattern,
//             &guide_in_extend_edge,
//             0,
//             &player_src_record,
//         )
//         .unwrap();
//         assert_eq!(coaches_from_player_guide.len(), 100);
//         let fans_from_player_loved_by = get_adj_vertices_set(
//             &sample_graph,
//             &player_src_pattern,
//             &loved_by_out_extend_edge,
//             2,
//             &player_src_record,
//         )
//         .unwrap();
//         assert_eq!(fans_from_player_loved_by.len(), 100);
//         let players_from_fan_loved_by = get_adj_vertices_set(
//             &sample_graph,
//             &fan_src_pattern,
//             &loved_by_in_extend_edge,
//             1,
//             &fan_src_record,
//         )
//         .unwrap();
//         assert_eq!(players_from_fan_loved_by.len(), 100);
//         let tickets_from_fan_buy =
//             get_adj_vertices_set(&sample_graph, &fan_src_pattern, &buy_out_extend_edge, 3, &fan_src_record)
//                 .unwrap();
//         assert_eq!(tickets_from_fan_buy.len(), 1);
//         let fans_from_ticket_buy = get_adj_vertices_set(
//             &sample_graph,
//             &ticket_src_pattern,
//             &buy_in_extend_edge,
//             2,
//             &ticket_src_record,
//         )
//         .unwrap();
//         assert_eq!(fans_from_ticket_buy.len(), 1);
//     }

//     #[test]
//     fn test_get_adj_vertices_sets() {
//         let sample_graph = load_sample_graph("../core/resource/test_graph");
//         let coach_src_record = get_src_records_from_label(&sample_graph, 0)[0].clone();
//         let coach_src_pattern = Pattern::from(PatternVertex::new(0, 0));
//         let guide_out_extend_step = ExtendStep::new(1, vec![ExtendEdge::new(0, 0, PatternDirection::Out)]);
//         let player_sets = get_adj_vertices_sets(
//             &sample_graph,
//             &coach_src_pattern,
//             &guide_out_extend_step,
//             &coach_src_record,
//         )
//         .unwrap();
//         assert_eq!(player_sets.len(), 1);
//         assert_eq!(player_sets[0].1.len(), 100);
//     }

//     #[test]
//     fn test_intersect_adj_vertices_sets() {
//         let sample_graph = load_sample_graph("../core/resource/test_graph");
//         let coach_src_pattern = Pattern::from(PatternVertex::new(0, 0));
//         let guide_out_extend_step = ExtendStep::new(1, vec![ExtendEdge::new(0, 0, PatternDirection::Out)]);
//         let coach_src_record_0 = get_src_records_from_label(&sample_graph, 0)[0].clone();
//         let coach_src_record_1 = get_src_records_from_label(&sample_graph, 0)[50].clone();
//         let mut player_sets_0 = get_adj_vertices_sets(
//             &sample_graph,
//             &coach_src_pattern,
//             &guide_out_extend_step,
//             &coach_src_record_0,
//         )
//         .unwrap();
//         let mut player_sets_1 = get_adj_vertices_sets(
//             &sample_graph,
//             &coach_src_pattern,
//             &guide_out_extend_step,
//             &coach_src_record_1,
//         )
//         .unwrap();
//         player_sets_0.append(&mut player_sets_1);
//         assert_eq!(intersect_adj_vertices_sets(player_sets_0).len(), 50);
//     }

//     #[test]
//     fn test_sample_records() {
//         let sample_graph = load_sample_graph("../core/resource/test_graph");
//         let mut coach_src_records = get_src_records_from_label(&sample_graph, 0);
//         let rate = 0.35;
//         coach_src_records = sample_records(coach_src_records, rate, None);
//         assert_eq!(coach_src_records.len(), 3500);
//     }

//     #[test]
//     fn test_build_and_update_catalog() {
//         let coach_vertex = PatternVertex::new(0, 0);
//         let player_vertex = PatternVertex::new(1, 1);
//         let fan_vertex = PatternVertex::new(2, 2);
//         let ticket_vertex = PatternVertex::new(3, 3);
//         let coach_guide_player_edge = PatternEdge::new(0, 0, coach_vertex, player_vertex);
//         let player_lovded_by_fan_edge = PatternEdge::new(1, 1, player_vertex, fan_vertex);
//         let fan_buy_ticket_edge = PatternEdge::new(2, 2, fan_vertex, ticket_vertex);
//         let pattern = Pattern::try_from(vec![
//             coach_guide_player_edge,
//             player_lovded_by_fan_edge,
//             fan_buy_ticket_edge,
//         ])
//         .unwrap();
//         let mut catalog = Catalogue::build_from_pattern(&pattern);
//         assert_eq!(catalog.get_patterns_num(), 10);
//         assert_eq!(catalog.get_approaches_num(), 12);
//         let sample_graph = Arc::new(load_sample_graph("../core/resource/test_graph"));
//         let pattern_meta = load_pattern_meta("../core/resource/test_graph/graph_schema/schema.json");
//         catalog.estimate_graph(Arc::clone(&sample_graph), 0.1, Some(10000));
//         println!("{:?}", pattern.generate_optimized_match_plan_greedily(&catalog, &pattern_meta, false));
//     }

//     #[test]
//     fn test_get_src_records_from_extend_steps() {
//         let coach_vertex = PatternVertex::new(0, 0);
//         let player_vertex = PatternVertex::new(1, 1);
//         let fan_vertex = PatternVertex::new(2, 2);
//         let ticket_vertex = PatternVertex::new(3, 3);
//         let coach_guide_player_edge = PatternEdge::new(0, 0, coach_vertex, player_vertex);
//         let player_lovded_by_fan_edge = PatternEdge::new(1, 1, player_vertex, fan_vertex);
//         let fan_buy_ticket_edge = PatternEdge::new(2, 2, fan_vertex, ticket_vertex);
//         let pattern = Pattern::try_from(vec![
//             coach_guide_player_edge,
//             player_lovded_by_fan_edge,
//             fan_buy_ticket_edge,
//         ])
//         .unwrap();
//         let mut catalog = Catalogue::build_from_pattern(&pattern);
//         let sample_graph = Arc::new(load_sample_graph("../core/resource/test_graph"));
//         catalog.estimate_graph(Arc::clone(&sample_graph), 0.1, Some(10000));
//         let pattern_index = catalog
//             .get_pattern_index(&pattern.encode_to())
//             .unwrap();
//         let (extend_steps, _) = get_definite_extend_steps_recursively(&mut catalog, pattern_index, pattern);
//         let pattern_records = get_src_records(&sample_graph, extend_steps, None);
//         assert_eq!(pattern_records.len(), 1000000);
//     }

//     #[test]
//     fn test_update_catalog_from_new_pattern() {
//         let coach_vertex = PatternVertex::new(0, 0);
//         let player_vertex = PatternVertex::new(1, 1);
//         let fan_vertex = PatternVertex::new(2, 2);
//         let ticket_vertex = PatternVertex::new(3, 3);
//         let coach_guide_player_edge = PatternEdge::new(0, 0, coach_vertex, player_vertex);
//         let player_lovded_by_fan_edge = PatternEdge::new(1, 1, player_vertex, fan_vertex);
//         let fan_buy_ticket_edge = PatternEdge::new(2, 2, fan_vertex, ticket_vertex);
//         let pattern1 =
//             Pattern::try_from(vec![coach_guide_player_edge.clone(), player_lovded_by_fan_edge.clone()])
//                 .unwrap();
//         let pattern2 = Pattern::try_from(vec![
//             coach_guide_player_edge,
//             player_lovded_by_fan_edge,
//             fan_buy_ticket_edge,
//         ])
//         .unwrap();
//         let sample_graph = Arc::new(load_sample_graph("../core/resource/test_graph"));
//         let mut catalog = Catalogue::build_from_pattern(&pattern1);
//         catalog.estimate_graph(Arc::clone(&sample_graph), 0.1, Some(10000));
//         catalog.update_catalog_by_pattern(&pattern2);
//         assert_eq!(catalog.get_patterns_num(), 10);
//         assert_eq!(catalog.get_approaches_num(), 12);
//         catalog.estimate_graph(Arc::clone(&sample_graph), 0.1, Some(10000));
//     }
// }
