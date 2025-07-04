//
//! Copyright 2021 Alibaba Group Holding Limited.
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

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use ahash::{HashMap, HashMapExt};
use dyn_type::{object, Object};
use graph_store::common::LabelId as StoreLabelId;
use graph_store::config::{JsonConf, DIR_GRAPH_SCHEMA, FILE_SCHEMA};
use graph_store::ldbc::{LDBCVertexParser, LABEL_SHIFT_BITS};
use graph_store::prelude::{
    DefaultId, EdgeId, GlobalStoreTrait, GlobalStoreUpdate, GraphDBConfig, InternalId, LDBCGraphSchema,
    LargeGraphDB, LocalEdge, LocalVertex, MutableGraphDB, Row, INVALID_LABEL_ID,
};
use ir_common::{KeyId, LabelId, NameOrId};
use pegasus::configure_with_default;
use pegasus_common::downcast::*;
use pegasus_common::impl_as_any;
use rand::prelude::StdRng;
use rand::{Rng, SeedableRng};

use crate::apis::graph::PKV;
use crate::apis::{
    from_fn, register_graph, Details, Direction, DynDetails, Edge, PropertyValue, QueryParams, ReadGraph,
    Statement, Vertex, ID,
};
use crate::errors::{GraphProxyError, GraphProxyResult};
use crate::{filter_limit, filter_sample_limit, limit_n, sample_limit};

const EXP_STORE_PK: KeyId = 0;

lazy_static! {
    pub static ref DATA_PATH: String = configure_with_default!(String, "DATA_PATH", "".to_string());
    pub static ref PARTITION_ID: usize = configure_with_default!(usize, "PARTITION_ID", 0);
    pub static ref GRAPH: LargeGraphDB<DefaultId, InternalId> = _init_graph();
    static ref GRAPH_PROXY: Arc<ExpStore> = initialize();
}

pub struct ExpStore {
    store: &'static LargeGraphDB<DefaultId, InternalId>,
}

fn initialize() -> Arc<ExpStore> {
    lazy_static::initialize(&GRAPH);
    Arc::new(ExpStore { store: &GRAPH })
}

fn _init_graph() -> LargeGraphDB<DefaultId, InternalId> {
    if DATA_PATH.is_empty() {
        info!("Create and use the modern graph for demo.");
        _init_modern_graph()
    } else {
        info!("Read the graph data from {:?} for demo.", *DATA_PATH);
        GraphDBConfig::default()
            .root_dir(&(*DATA_PATH))
            .partition(*PARTITION_ID)
            .schema_file(
                &(DATA_PATH.as_ref() as &Path)
                    .join(DIR_GRAPH_SCHEMA)
                    .join(FILE_SCHEMA),
            )
            .open()
            .expect("Open graph error")
    }
}

fn _init_modern_graph() -> LargeGraphDB<DefaultId, InternalId> {
    let mut mut_graph: MutableGraphDB<DefaultId, InternalId> = GraphDBConfig::default().new();

    let v1: DefaultId = LDBCVertexParser::to_global_id(1, 0);
    let v2: DefaultId = LDBCVertexParser::to_global_id(2, 0);
    let v3: DefaultId = LDBCVertexParser::to_global_id(3, 1);
    let v4: DefaultId = LDBCVertexParser::to_global_id(4, 0);
    let v5: DefaultId = LDBCVertexParser::to_global_id(5, 1);
    let v6: DefaultId = LDBCVertexParser::to_global_id(6, 0);

    mut_graph.add_vertex(v1, [0, INVALID_LABEL_ID]);
    mut_graph.add_vertex(v2, [0, INVALID_LABEL_ID]);
    mut_graph.add_vertex(v3, [1, INVALID_LABEL_ID]);
    mut_graph.add_vertex(v4, [0, INVALID_LABEL_ID]);
    mut_graph.add_vertex(v5, [1, INVALID_LABEL_ID]);
    mut_graph.add_vertex(v6, [0, INVALID_LABEL_ID]);

    let prop7 = Row::from(vec![object!(0.5)]);
    let prop8 = Row::from(vec![object!(0.4)]);
    let prop9 = Row::from(vec![object!(1.0)]);
    let prop10 = Row::from(vec![object!(0.4)]);
    let prop11 = Row::from(vec![object!(1.0)]);
    let prop12 = Row::from(vec![object!(0.2)]);

    mut_graph
        .add_edge_with_properties(v1, v2, 0, prop7)
        .unwrap();
    mut_graph
        .add_edge_with_properties(v1, v3, 1, prop8)
        .unwrap();
    mut_graph
        .add_edge_with_properties(v1, v4, 0, prop9)
        .unwrap();
    mut_graph
        .add_edge_with_properties(v4, v3, 1, prop10)
        .unwrap();
    mut_graph
        .add_edge_with_properties(v4, v5, 1, prop11)
        .unwrap();
    mut_graph
        .add_edge_with_properties(v6, v3, 1, prop12)
        .unwrap();

    let prop1 = Row::from(vec![object!(1), object!("marko"), object!(29)]);
    let prop2 = Row::from(vec![object!(2), object!("vadas"), object!(27)]);
    let prop3 = Row::from(vec![object!(3), object!("lop"), object!("java")]);
    let prop4 = Row::from(vec![object!(4), object!("josh"), object!(32)]);
    let prop5 = Row::from(vec![object!(5), object!("ripple"), object!("java")]);
    let prop6 = Row::from(vec![object!(6), object!("peter"), object!(35)]);

    mut_graph
        .add_or_update_vertex_properties(v1, prop1)
        .unwrap();
    mut_graph
        .add_or_update_vertex_properties(v2, prop2)
        .unwrap();
    mut_graph
        .add_or_update_vertex_properties(v3, prop3)
        .unwrap();
    mut_graph
        .add_or_update_vertex_properties(v4, prop4)
        .unwrap();
    mut_graph
        .add_or_update_vertex_properties(v5, prop5)
        .unwrap();
    mut_graph
        .add_or_update_vertex_properties(v6, prop6)
        .unwrap();

    let modern_graph_schema = r#"
    {
      "vertex_type_map": {
        "person": 0,
        "software": 1
      },
      "edge_type_map": {
        "knows": 0,
        "created": 1
      },
      "vertex_prop": {
        "person": [
          [
            "id",
            "ID"
          ],
          [
            "name",
            "String"
          ],
          [
            "age",
            "Integer"
          ]
        ],
        "software": [
          [
            "id",
            "ID"
          ],
          [
            "name",
            "String"
          ],
          [
            "lang",
            "String"
          ]
        ]
      },
      "edge_prop": {
        "knows": [
          [
            "start_id",
            "ID"
          ],
          [
            "end_id",
            "ID"
          ],
          [
            "weight",
            "Double"
          ]
        ],
        "created": [
          [
            "start_id",
            "ID"
          ],
          [
            "end_id",
            "ID"
          ],
          [
            "weight",
            "Double"
          ]
        ]
      }
    }
    "#;
    let schema = LDBCGraphSchema::from_json(modern_graph_schema.to_string()).expect("Parse schema error!");

    mut_graph.into_graph(schema)
}

impl ReadGraph for ExpStore {
    fn scan_vertex(
        &self, params: &QueryParams,
    ) -> GraphProxyResult<Box<dyn Iterator<Item = Vertex> + Send>> {
        // DemoGraph contains a single graph partition on each server,
        // therefore, there's no need to use the specific partition id for query.
        // Besides, workers will scan the vertices in a parallel way
        let label_ids = encode_storage_label(&params.labels);
        let props = params.columns.clone();

        // get_current_worker_checked() in case pegasus not started, i.e., for ci tests.
        let worker_id = pegasus::get_current_worker_checked()
            .map(|worker| worker.index)
            .unwrap_or(0);
        let workers_num = pegasus::get_current_worker_checked()
            .map(|worker| worker.local_peers)
            .unwrap_or(1);
        let count = self
            .store
            .count_all_vertices(label_ids.as_ref());
        let partial_count = count / workers_num as usize;
        let take_count = if (worker_id + 1) % workers_num == 0 {
            count - partial_count * (workers_num as usize - 1)
        } else {
            partial_count
        };

        let result = self
            .store
            .get_all_vertices(label_ids.as_ref())
            .skip((worker_id % workers_num) as usize * partial_count)
            .take(take_count)
            .map(move |v| to_runtime_vertex(v, props.clone()));

        Ok(filter_sample_limit!(result, params.filter, params.sample_ratio, params.limit))
    }

    fn index_scan_vertex(
        &self, _label: LabelId, _primary_key: &PKV, _params: &QueryParams,
    ) -> GraphProxyResult<Option<Vertex>> {
        Err(GraphProxyError::unsupported_error(
            "Experiment storage does not support index_scan_vertex for now",
        ))?
    }

    fn scan_edge(&self, params: &QueryParams) -> GraphProxyResult<Box<dyn Iterator<Item = Edge> + Send>> {
        // DemoGraph contains a single graph partition on each server,
        // therefore, there's no need to use the specific partition id for query.
        // Besides, workers will scan the edges in a parallel way
        let label_ids = encode_storage_label(&params.labels);
        let props = params.columns.clone();

        // get_current_worker_checked() in case pegasus not started, i.e., for ci tests.
        let worker_id = pegasus::get_current_worker_checked()
            .map(|worker| worker.index)
            .unwrap_or(0);
        let workers_num = pegasus::get_current_worker_checked()
            .map(|worker| worker.local_peers)
            .unwrap_or(1);
        let count = self.store.count_all_edges(label_ids.as_ref());
        let partial_count = count / workers_num as usize;
        let take_count = if (worker_id + 1) % workers_num == 0 {
            count - partial_count * (workers_num as usize - 1)
        } else {
            partial_count
        };

        let result = self
            .store
            .get_all_edges(label_ids.as_ref())
            .skip((worker_id % workers_num) as usize * partial_count)
            .take(take_count)
            .map(move |v| to_runtime_edge(v, props.clone()));

        Ok(filter_sample_limit!(result, params.filter, params.sample_ratio, params.limit))
    }

    fn get_vertex(
        &self, ids: &[ID], params: &QueryParams,
    ) -> GraphProxyResult<Box<dyn Iterator<Item = Vertex> + Send>> {
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(local_vertex) = self.store.get_vertex(*id as DefaultId) {
                let v = to_runtime_vertex(local_vertex, params.columns.clone());
                result.push(v);
            }
        }
        Ok(filter_limit!(result.into_iter(), params.filter, params.limit))
    }

    fn get_edge(
        &self, ids: &[ID], params: &QueryParams,
    ) -> GraphProxyResult<Box<dyn Iterator<Item = Edge> + Send>> {
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let eid = encode_store_e_id(id);
            if let Some(local_edge) = self.store.get_edge(eid) {
                let e = to_runtime_edge(local_edge, params.columns.clone());
                result.push(e);
            }
        }
        Ok(filter_limit!(result.into_iter(), params.filter, params.limit))
    }

    fn prepare_explore_vertex(
        &self, direction: Direction, params: &QueryParams,
    ) -> GraphProxyResult<Box<dyn Statement<ID, Vertex>>> {
        let edge_label_ids = encode_storage_label(params.labels.as_ref());
        let filter = params.filter.clone();
        let limit = params.limit.clone();
        let graph = self.store;

        let stmt = from_fn(move |v: ID| {
            let iter = match direction {
                Direction::Out => graph.get_out_vertices(v as DefaultId, edge_label_ids.as_ref()),
                Direction::In => graph.get_in_vertices(v as DefaultId, edge_label_ids.as_ref()),
                Direction::Both => graph.get_both_vertices(v as DefaultId, edge_label_ids.as_ref()),
            }
            .map(move |v| to_empty_vertex(v));
            Ok(filter_limit!(iter, filter, limit))
        });
        Ok(stmt)
    }

    fn prepare_explore_edge(
        &self, direction: Direction, params: &QueryParams,
    ) -> GraphProxyResult<Box<dyn Statement<ID, Edge>>> {
        let edge_label_ids = encode_storage_label(&params.labels);
        let filter = params.filter.clone();
        let limit = params.limit.clone();
        let graph = self.store;
        let props = params.columns.clone();

        let stmt = from_fn(move |v: ID| {
            let props = props.clone();
            let iter = match direction {
                Direction::Out => graph.get_out_edges(v as DefaultId, edge_label_ids.as_ref()),
                Direction::In => graph.get_in_edges(v as DefaultId, edge_label_ids.as_ref()),
                Direction::Both => graph.get_both_edges(v as DefaultId, edge_label_ids.as_ref()),
            }
            .map(move |e| to_runtime_edge(e, props.clone()));
            Ok(filter_limit!(iter, filter, limit))
        });
        Ok(stmt)
    }

    fn get_primary_key(&self, id: &ID) -> GraphProxyResult<Option<PKV>> {
        let outer_id = (*id << LABEL_SHIFT_BITS) >> LABEL_SHIFT_BITS;
        let pk_val = Object::from(outer_id);
        Ok(Some((EXP_STORE_PK.into(), pk_val).into()))
    }
}

#[allow(dead_code)]
pub fn create_exp_store() {
    lazy_static::initialize(&GRAPH_PROXY);
    register_graph(GRAPH_PROXY.clone());
}

#[inline]
fn to_runtime_vertex(v: LocalVertex<'static, DefaultId>, prop_keys: Option<Vec<NameOrId>>) -> Vertex {
    // For vertices, we query properties via vid
    let id = v.get_id() as ID;
    let label = encode_runtime_v_label(&v);
    let details = LazyVertexDetails::new(v, prop_keys);
    Vertex::new(id, Some(label), DynDetails::lazy(details))
}

#[inline]
fn to_empty_vertex(v: LocalVertex<'static, DefaultId>) -> Vertex {
    let id = v.get_id() as ID;
    let label = encode_runtime_v_label(&v);
    Vertex::new(id, Some(label), DynDetails::default())
}

#[inline]
fn to_runtime_edge(e: LocalEdge<'static, DefaultId, InternalId>, prop_keys: Option<Vec<NameOrId>>) -> Edge {
    let id = encode_runtime_e_id(&e);
    let label = encode_runtime_e_label(&e);
    let src_id = e.get_src_id();
    let dst_id = e.get_dst_id();
    let from_src = e.is_from_start();
    let details = LazyEdgeDetails::new(e, prop_keys);
    let store_src_label: StoreLabelId = (src_id >> LABEL_SHIFT_BITS) as StoreLabelId;
    let store_dst_label: StoreLabelId = (dst_id >> LABEL_SHIFT_BITS) as StoreLabelId;
    let src_label = encode_runtime_label(store_src_label);
    let dst_label = encode_runtime_label(store_dst_label);

    let mut e = Edge::with_from_src(
        id,
        Some(label),
        src_id as ID,
        dst_id as ID,
        from_src,
        DynDetails::lazy(details),
    );

    e.set_src_label(src_label);
    e.set_dst_label(dst_label);
    e
}

/// LazyVertexDetails is used for local property fetching optimization.
/// That is, the required properties will not be materialized until LazyVertexDetails need to be shuffled.
#[allow(dead_code)]
struct LazyVertexDetails {
    // prop_keys specify the properties we would save for later queries after shuffle,
    // excluding the ones used only when local property fetching.
    // Specifically, Some(vec![]) indicates we need all properties
    // and None indicates we do not need any property
    prop_keys: Option<Vec<NameOrId>>,
    inner: AtomicPtr<LocalVertex<'static, DefaultId>>,
}

impl_as_any!(LazyVertexDetails);

impl LazyVertexDetails {
    pub fn new(v: LocalVertex<'static, DefaultId>, prop_keys: Option<Vec<NameOrId>>) -> Self {
        let ptr = Box::into_raw(Box::new(v));
        LazyVertexDetails { prop_keys, inner: AtomicPtr::new(ptr) }
    }

    fn get_vertex_ptr(&self) -> Option<*mut LocalVertex<'static, DefaultId>> {
        let ptr = self.inner.load(Ordering::SeqCst);
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }
}

impl fmt::Debug for LazyVertexDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LazyVertexDetails")
            .field("properties", &self.prop_keys)
            .field("inner", &self.inner)
            .finish()
    }
}

impl Details for LazyVertexDetails {
    fn get_property(&self, key: &NameOrId) -> Option<PropertyValue> {
        if let NameOrId::Str(key) = key {
            if let Some(ptr) = self.get_vertex_ptr() {
                unsafe {
                    (*ptr)
                        .get_property(key)
                        .map(|prop| PropertyValue::Borrowed(prop))
                }
            } else {
                None
            }
        } else {
            info!("Have not support getting property by prop_id in exp_store yet");
            None
        }
    }

    fn get_all_properties(&self) -> Option<HashMap<NameOrId, Object>> {
        let mut all_props = HashMap::new();
        if let Some(prop_keys) = self.prop_keys.as_ref() {
            // the case of get_all_properties from vertex;
            if prop_keys.is_empty() {
                if let Some(ptr) = self.get_vertex_ptr() {
                    unsafe {
                        if let Some(prop_key_vals) = (*ptr).clone_all_properties() {
                            all_props = prop_key_vals
                                .into_iter()
                                .map(|(prop_key, prop_val)| (prop_key.into(), prop_val as Object))
                                .collect();
                        } else {
                            return None;
                        }
                    }
                } else {
                    return None;
                }
            } else {
                // the case of get_all_properties with prop_keys pre-specified
                for key in prop_keys.iter() {
                    if let Some(prop) = self.get_property(&key) {
                        all_props.insert(key.clone(), prop.try_to_owned().unwrap());
                    } else {
                        all_props.insert(key.clone(), Object::None);
                    }
                }
            }
        }
        Some(all_props)
    }

    fn insert_property(&mut self, key: NameOrId, _value: Object) {
        if let Some(prop_keys) = self.prop_keys.as_mut() {
            if !prop_keys.is_empty() {
                prop_keys.push(key);
            }
        } else {
            self.prop_keys = Some(vec![key]);
        }
    }
}

impl Drop for LazyVertexDetails {
    fn drop(&mut self) {
        let ptr = self.inner.load(Ordering::SeqCst);
        if !ptr.is_null() {
            unsafe {
                std::ptr::drop_in_place(ptr);
            }
        }
    }
}

/// LazyEdgeDetails is used for local property fetching optimization.
/// That is, the required properties will not be materialized until LazyEdgeDetails need to be shuffled.
#[allow(dead_code)]
struct LazyEdgeDetails {
    // prop_keys specify the properties we would save for later queries after shuffle,
    // excluding the ones used only when local property fetching.
    // Specifically, Some(vec![]) indicates we need all properties
    // and None indicates we do not need any property,
    prop_keys: Option<Vec<NameOrId>>,
    inner: AtomicPtr<LocalEdge<'static, DefaultId, InternalId>>,
}

impl_as_any!(LazyEdgeDetails);

impl LazyEdgeDetails {
    pub fn new(e: LocalEdge<'static, DefaultId, InternalId>, prop_keys: Option<Vec<NameOrId>>) -> Self {
        let ptr = Box::into_raw(Box::new(e));
        LazyEdgeDetails { prop_keys, inner: AtomicPtr::new(ptr) }
    }

    fn get_edge_ptr(&self) -> Option<*mut LocalEdge<'static, DefaultId, InternalId>> {
        let ptr = self.inner.load(Ordering::SeqCst);
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }
}

impl fmt::Debug for LazyEdgeDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LazyEdgeDetails")
            .field("prop_keys", &self.prop_keys)
            .field("inner", &self.inner)
            .finish()
    }
}

impl Details for LazyEdgeDetails {
    fn get_property(&self, key: &NameOrId) -> Option<PropertyValue> {
        if let NameOrId::Str(key) = key {
            let ptr = self.get_edge_ptr();
            if let Some(ptr) = ptr {
                unsafe {
                    (*ptr)
                        .get_property(key)
                        .map(|prop| PropertyValue::Borrowed(prop))
                }
            } else {
                None
            }
        } else {
            info!("Have not support getting property by prop_id in experiments store yet");
            None
        }
    }

    fn get_all_properties(&self) -> Option<HashMap<NameOrId, Object>> {
        let mut all_props = HashMap::new();
        if let Some(prop_keys) = self.prop_keys.as_ref() {
            // the case of get_all_properties from vertex;
            if prop_keys.is_empty() {
                let ptr = self.get_edge_ptr();
                if let Some(ptr) = ptr {
                    unsafe {
                        if let Some(prop_key_vals) = (*ptr).clone_all_properties() {
                            all_props = prop_key_vals
                                .into_iter()
                                .map(|(prop_key, prop_val)| (prop_key.into(), prop_val as Object))
                                .collect();
                        } else {
                            return None;
                        }
                    }
                } else {
                    return None;
                }
            } else {
                // the case of get_all_properties with prop_keys pre-specified
                for key in prop_keys.iter() {
                    if let Some(prop) = self.get_property(&key) {
                        all_props.insert(key.clone(), prop.try_to_owned().unwrap());
                    } else {
                        all_props.insert(key.clone(), Object::None);
                    }
                }
            }
        }
        Some(all_props)
    }

    fn insert_property(&mut self, key: NameOrId, _value: Object) {
        if let Some(prop_keys) = self.prop_keys.as_mut() {
            if !prop_keys.is_empty() {
                prop_keys.push(key);
            }
        } else {
            self.prop_keys = Some(vec![key]);
        }
    }
}

impl Drop for LazyEdgeDetails {
    fn drop(&mut self) {
        let ptr = self.inner.load(Ordering::SeqCst);
        if !ptr.is_null() {
            unsafe {
                std::ptr::drop_in_place(ptr);
            }
        }
    }
}

/// Edge's ID is encoded by its internal index
#[inline]
fn encode_runtime_e_id(e: &LocalEdge<DefaultId, InternalId>) -> ID {
    let ei = e.get_edge_id();
    ei.1 as ID
}

#[inline]
pub fn encode_store_e_id(e: &ID) -> EdgeId<DefaultId> {
    // TODO(longbin) To only use in current partition
    (0, *e as usize)
}

#[inline]
fn encode_runtime_label(l: StoreLabelId) -> LabelId {
    l as LabelId
}

#[inline]
fn encode_runtime_v_label(v: &LocalVertex<DefaultId>) -> LabelId {
    // exp_store has a hierarchical (two-layer) label structure,
    // e.g., [PERSON, INVALID_LABEL_ID]; or [PLACE, CITY].
    // Currently, we visit either "PERSON", or "CITY".
    // if v.get_label()[1] == INVALID_LABEL_ID {
    //     encode_runtime_label(v.get_label()[0])
    // } else {
    //     encode_runtime_label(v.get_label()[1])
    // }
    encode_runtime_label(v.get_label()[0])
}

#[inline]
fn encode_runtime_e_label(e: &LocalEdge<DefaultId, InternalId>) -> LabelId {
    encode_runtime_label(e.get_label())
}

/// Transform string-typed labels into a id-typed labels.
/// `is_true_label` records whether the label is an actual label, or already transformed into
/// an id-type.
#[inline]
fn encode_storage_label(labels: &Vec<LabelId>) -> Option<Vec<StoreLabelId>> {
    if labels.is_empty() {
        None
    } else {
        Some(
            labels
                .iter()
                .map(|label| *label as StoreLabelId)
                .collect::<Vec<StoreLabelId>>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use graph_store::common::LabelId;
    use graph_store::ldbc::{LDBCVertexParser, LABEL_SHIFT_BITS};
    use graph_store::prelude::{DefaultId, GlobalStoreTrait};

    use super::GRAPH;

    #[test]
    fn it_works() {
        let v1: DefaultId = LDBCVertexParser::to_global_id(1, 0);
        let v2: DefaultId = LDBCVertexParser::to_global_id(2, 0);
        let v4: DefaultId = LDBCVertexParser::to_global_id(4, 0);

        let out_iter = GRAPH.get_out_vertices(v1, Some(&vec![0]));
        let mut out: Vec<DefaultId> = out_iter.map(|v| v.get_id()).collect();
        out.sort();

        assert_eq!(out, vec![v2, v4]);
    }

    #[test]
    fn label_test() {
        let v1: DefaultId = LDBCVertexParser::to_global_id(1, 0);
        let v2: DefaultId = LDBCVertexParser::to_global_id(2, 1);

        let v1_label: LabelId = (v1 >> LABEL_SHIFT_BITS) as LabelId;
        let v2_label: LabelId = (v2 >> LABEL_SHIFT_BITS) as LabelId;

        assert_eq!(v1_label, 0);
        assert_eq!(v2_label, 1);
    }
}
