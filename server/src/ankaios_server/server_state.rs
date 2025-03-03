// Copyright (c) 2024 Elektrobit Automotive GmbH
//
// This program and the accompanying materials are made available under the
// terms of the Apache License, Version 2.0 which is available at
// https://www.apache.org/licenses/LICENSE-2.0.
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the
// License for the specific language governing permissions and limitations
// under the License.
//
// SPDX-License-Identifier: Apache-2.0

use super::cycle_check;
#[cfg_attr(test, mockall_double::double)]
use super::delete_graph::DeleteGraph;
use crate::state_manipulation::{Object, Path};
use crate::workload_state_db::WorkloadStateDB;
use common::std_extensions::IllegalStateResult;
use common::{
    commands::{CompleteState, CompleteStateRequest},
    objects::{DeletedWorkload, State, WorkloadSpec},
};
use std::fmt::Display;

#[cfg(test)]
use mockall::automock;

fn update_state(
    current_state: &CompleteState,
    updated_state: CompleteState,
    update_mask: Vec<String>,
) -> Result<CompleteState, UpdateStateError> {
    // [impl->swdd~update-current-state-empty-update-mask~1]
    if update_mask.is_empty() {
        return Ok(updated_state);
    }

    // [impl->swdd~update-current-state-with-update-mask~1]
    let mut new_state: Object = current_state.try_into().map_err(|err| {
        UpdateStateError::ResultInvalid(format!("Failed to parse current state, '{}'", err))
    })?;
    let state_from_update: Object = updated_state.try_into().map_err(|err| {
        UpdateStateError::ResultInvalid(format!("Failed to parse new state, '{}'", err))
    })?;

    for field in update_mask {
        let field: Path = field.into();
        if let Some(field_from_update) = state_from_update.get(&field) {
            if new_state.set(&field, field_from_update.to_owned()).is_err() {
                return Err(UpdateStateError::FieldNotFound(field.into()));
            }
        } else if new_state.remove(&field).is_err() {
            return Err(UpdateStateError::FieldNotFound(field.into()));
        }
    }

    if let Ok(new_state) = new_state.try_into() {
        Ok(new_state)
    } else {
        Err(UpdateStateError::ResultInvalid(
            "Could not parse into CompleteState.".to_string(),
        ))
    }
}

fn extract_added_and_deleted_workloads(
    current_state: &State,
    new_state: &State,
) -> Option<(Vec<WorkloadSpec>, Vec<DeletedWorkload>)> {
    let mut added_workloads: Vec<WorkloadSpec> = Vec::new();
    let mut deleted_workloads: Vec<DeletedWorkload> = Vec::new();

    // find updated or deleted workloads
    current_state.workloads.iter().for_each(|(wl_name, wls)| {
        if let Some(new_wls) = new_state.workloads.get(wl_name) {
            // The new workload is identical with existing or updated. Lets check if it is an update.
            if wls != new_wls {
                // [impl->swdd~server-detects-changed-workload~1]
                added_workloads.push(new_wls.clone());
                deleted_workloads.push(DeletedWorkload {
                    agent: wls.agent.clone(),
                    name: wl_name.clone(),
                    ..Default::default()
                });
            }
        } else {
            // [impl->swdd~server-detects-deleted-workload~1]
            deleted_workloads.push(DeletedWorkload {
                agent: wls.agent.clone(),
                name: wl_name.clone(),
                ..Default::default()
            });
        }
    });

    // find new workloads
    // [impl->swdd~server-detects-new-workload~1]
    new_state
        .workloads
        .iter()
        .for_each(|(new_wl_name, new_wls)| {
            if !current_state.workloads.contains_key(new_wl_name) {
                added_workloads.push(new_wls.clone());
            }
        });

    if added_workloads.is_empty() && deleted_workloads.is_empty() {
        return None;
    }

    Some((added_workloads, deleted_workloads))
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStateError {
    FieldNotFound(String),
    ResultInvalid(String),
    CycleInDependencies(String),
}

impl Display for UpdateStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateStateError::FieldNotFound(field) => {
                write!(f, "Could not find field {}", field)
            }
            UpdateStateError::ResultInvalid(reason) => {
                write!(f, "Resulting State is invalid, reason: '{}'", reason)
            }
            UpdateStateError::CycleInDependencies(workload_part_of_cycle) => {
                write!(
                    f,
                    "workload dependency '{}' is part of a cycle.",
                    workload_part_of_cycle
                )
            }
        }
    }
}

#[derive(Default)]
pub struct ServerState {
    state: CompleteState,
    delete_graph: DeleteGraph,
}

pub type AddedDeletedWorkloads = Option<(Vec<WorkloadSpec>, Vec<DeletedWorkload>)>;

#[cfg_attr(test, automock)]
impl ServerState {
    pub fn get_complete_state_by_field_mask(
        &self,
        request_complete_state: &CompleteStateRequest,
        workload_state_db: &WorkloadStateDB,
    ) -> Result<CompleteState, String> {
        let current_complete_state = CompleteState {
            current_state: self.state.current_state.clone(),
            startup_state: self.state.startup_state.clone(),
            workload_states: workload_state_db.get_all_workload_states(),
        };

        // [impl->swdd~server-filters-get-complete-state-result~1]
        if !request_complete_state.field_mask.is_empty() {
            let current_complete_state: Object =
                current_complete_state.try_into().unwrap_or_illegal_state();
            let mut return_state = Object::default();

            for field in &request_complete_state.field_mask {
                if let Some(value) = current_complete_state.get(&field.into()) {
                    return_state.set(&field.into(), value.to_owned())?;
                } else {
                    log::debug!(
                        concat!(
                        "Result for CompleteState incomplete, as requested field does not exist:\n",

                        "   field: {}"),
                        field
                    );
                    continue;
                };
            }

            return_state.try_into().map_err(|err: serde_yaml::Error| {
                format!("The result for CompleteState is invalid: '{}'", err)
            })
        } else {
            Ok(current_complete_state)
        }
    }

    // [impl->swdd~agent-from-agent-field~1]
    pub fn get_workloads_for_agent(&self, agent_name: &String) -> Vec<WorkloadSpec> {
        self.state
            .current_state
            .workloads
            .clone()
            .into_values()
            .filter(|workload_spec| workload_spec.agent.eq(agent_name))
            .collect()
    }

    pub fn update(
        &mut self,
        new_state: CompleteState,
        update_mask: Vec<String>,
    ) -> Result<AddedDeletedWorkloads, UpdateStateError> {
        // [impl->swdd~update-current-state-with-update-mask~1]
        // [impl->swdd~update-current-state-empty-update-mask~1]
        match update_state(&self.state, new_state, update_mask) {
            Ok(new_state) => {
                let cmd = extract_added_and_deleted_workloads(
                    &self.state.current_state,
                    &new_state.current_state,
                );

                if let Some((added_workloads, mut deleted_workloads)) = cmd {
                    let start_nodes: Vec<&String> = added_workloads
                        .iter()
                        .filter_map(|w| {
                            if !w.dependencies.is_empty() {
                                Some(&w.name)
                            } else {
                                None
                            }
                        })
                        .collect();

                    // [impl->swdd~server-state-rejects-state-with-cyclic-dependencies~1]
                    if let Some(workload_part_of_cycle) =
                        cycle_check::dfs(&new_state.current_state, Some(start_nodes))
                    {
                        return Err(UpdateStateError::CycleInDependencies(
                            workload_part_of_cycle,
                        ));
                    }

                    // [impl->swdd~server-state-stores-delete-condition~1]
                    self.delete_graph.insert(&added_workloads);

                    // [impl->swdd~server-state-adds-delete-conditions-to-deleted-workload~1]
                    self.delete_graph
                        .apply_delete_conditions_to(&mut deleted_workloads);

                    self.state = new_state;
                    Ok(Some((added_workloads, deleted_workloads)))
                } else {
                    Ok(None)
                }
            }
            Err(error) => Err(error),
        }
    }
}

//////////////////////////////////////////////////////////////////////////////
//                 ########  #######    #########  #########                //
//                    ##     ##        ##             ##                    //
//                    ##     #####     #########      ##                    //
//                    ##     ##                ##     ##                    //
//                    ##     #######   #########      ##                    //
//////////////////////////////////////////////////////////////////////////////
#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use common::{
        commands::{CompleteState, CompleteStateRequest},
        objects::{DeletedWorkload, State, WorkloadSpec},
        test_utils::{generate_test_complete_state, generate_test_workload_spec_with_param},
    };

    use crate::{
        ankaios_server::{delete_graph::MockDeleteGraph, server_state::UpdateStateError},
        workload_state_db::WorkloadStateDB,
    };

    use super::ServerState;
    const AGENT_A: &str = "agent_A";
    const AGENT_B: &str = "agent_B";
    const WORKLOAD_NAME_1: &str = "workload_1";
    const WORKLOAD_NAME_2: &str = "workload_2";
    const WORKLOAD_NAME_3: &str = "workload_3";
    const WORKLOAD_NAME_4: &str = "workload_4";
    const RUNTIME: &str = "runtime";

    // [utest->swdd~server-filters-get-complete-state-result~1]
    #[test]
    fn utest_server_state_get_complete_state_by_field_mask_empty_mask() {
        let w1 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_1.to_string(),
            RUNTIME.to_string(),
        );

        let w2 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_2.to_string(),
            RUNTIME.to_string(),
        );

        let w3 = generate_test_workload_spec_with_param(
            AGENT_B.to_string(),
            WORKLOAD_NAME_3.to_string(),
            RUNTIME.to_string(),
        );

        let server_state = ServerState {
            state: generate_test_complete_state(vec![w1.clone(), w2.clone(), w3.clone()]),
            ..Default::default()
        };

        let request_complete_state = CompleteStateRequest { field_mask: vec![] };

        let mut workload_state_db = WorkloadStateDB::default();
        workload_state_db.insert(server_state.state.workload_states.clone());

        let mut complete_state = server_state
            .get_complete_state_by_field_mask(&request_complete_state, &workload_state_db)
            .unwrap();

        // result must be sorted because inside WorkloadStateDB the order of workload states is not preserved
        complete_state
            .workload_states
            .sort_by(|left, right| left.workload_name.cmp(&right.workload_name));

        let mut expected_complete_state = server_state.state.clone();
        expected_complete_state
            .workload_states
            .sort_by(|left, right| left.workload_name.cmp(&right.workload_name));
        assert_eq!(expected_complete_state, complete_state);
    }

    // [utest->swdd~server-filters-get-complete-state-result~1]
    #[test]
    fn utest_server_state_get_complete_state_by_field_mask() {
        let w1 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_1.to_string(),
            RUNTIME.to_string(),
        );

        let w2 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_2.to_string(),
            RUNTIME.to_string(),
        );

        let w3 = generate_test_workload_spec_with_param(
            AGENT_B.to_string(),
            WORKLOAD_NAME_3.to_string(),
            RUNTIME.to_string(),
        );

        let server_state = ServerState {
            state: generate_test_complete_state(vec![w1.clone(), w2.clone(), w3.clone()]),
            ..Default::default()
        };

        let request_complete_state = CompleteStateRequest {
            field_mask: vec![
                format!("currentState.workloads.{}", WORKLOAD_NAME_1),
                format!("currentState.workloads.{}.agent", WORKLOAD_NAME_3),
            ],
        };

        let mut workload_state_db = WorkloadStateDB::default();
        workload_state_db.insert(server_state.state.workload_states.clone());

        let mut complete_state = server_state
            .get_complete_state_by_field_mask(&request_complete_state, &workload_state_db)
            .unwrap();

        // result must be sorted because inside WorkloadStateDB the order of workload states is not preserved
        complete_state
            .workload_states
            .sort_by(|left, right| left.workload_name.cmp(&right.workload_name));

        let mut expected_complete_state = server_state.state.clone();
        expected_complete_state.current_state.workloads = HashMap::from([
            (w1.name.clone(), w1.clone()),
            (
                w3.name.clone(),
                WorkloadSpec {
                    agent: AGENT_B.to_string(),
                    ..Default::default()
                },
            ),
        ]);
        expected_complete_state.workload_states.clear();
        assert_eq!(expected_complete_state, complete_state);
    }

    // [utest->swdd~server-filters-get-complete-state-result~1]
    #[test]
    fn utest_server_state_get_complete_state_by_field_mask_continue_on_invalid_mask() {
        let w1 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_1.to_string(),
            RUNTIME.to_string(),
        );

        let server_state = ServerState {
            state: generate_test_complete_state(vec![w1.clone()]),
            ..Default::default()
        };

        let request_complete_state = CompleteStateRequest {
            field_mask: vec![
                "workloads.invalidMask".to_string(), // invalid not existing workload
                format!("currentState.workloads.{}", WORKLOAD_NAME_1),
            ],
        };

        let mut workload_state_db = WorkloadStateDB::default();
        workload_state_db.insert(server_state.state.workload_states.clone());

        let mut complete_state = server_state
            .get_complete_state_by_field_mask(&request_complete_state, &workload_state_db)
            .unwrap();

        // result must be sorted because inside WorkloadStateDB the order of workload states is not preserved
        complete_state
            .workload_states
            .sort_by(|left, right| left.workload_name.cmp(&right.workload_name));

        let mut expected_complete_state = server_state.state.clone();
        expected_complete_state.current_state.workloads =
            HashMap::from([(w1.name.clone(), w1.clone())]);
        expected_complete_state.workload_states.clear();
        assert_eq!(expected_complete_state, complete_state);
    }

    // [utest->swdd~agent-from-agent-field~1]
    #[test]
    fn utest_server_state_get_workloads_per_agent() {
        let w1 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_1.to_string(),
            RUNTIME.to_string(),
        );

        let w2 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_2.to_string(),
            RUNTIME.to_string(),
        );

        let w3 = generate_test_workload_spec_with_param(
            AGENT_B.to_string(),
            WORKLOAD_NAME_3.to_string(),
            RUNTIME.to_string(),
        );

        let server_state = ServerState {
            state: generate_test_complete_state(vec![w1.clone(), w2.clone(), w3.clone()]),
            ..Default::default()
        };

        let mut workloads = server_state.get_workloads_for_agent(&AGENT_A.to_string());
        workloads.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(workloads, vec![w1, w2]);

        let workloads = server_state.get_workloads_for_agent(&AGENT_B.to_string());
        assert_eq!(workloads, vec![w3]);

        let workloads = server_state.get_workloads_for_agent(&"unknown_agent".to_string());
        assert_eq!(workloads.len(), 0);
    }

    // [utest->swdd~server-state-rejects-state-with-cyclic-dependencies~1]
    #[test]
    fn utest_server_state_update_state_reject_state_with_cyclic_dependencies() {
        let _ = env_logger::builder().is_test(true).try_init();

        let workload = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_1.to_string(),
            RUNTIME.to_string(),
        );

        // workload has a self cycle to workload A
        let new_workload_1 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            "workload A".to_string(),
            RUNTIME.to_string(),
        );

        let mut new_workload_2 = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_1.to_string(),
            RUNTIME.to_string(),
        );
        new_workload_2.dependencies.clear();

        let old_state = CompleteState {
            current_state: State {
                workloads: HashMap::from([(workload.name.clone(), workload)]),
                ..Default::default()
            },
            ..Default::default()
        };

        let rejected_new_state = CompleteState {
            current_state: State {
                workloads: HashMap::from([
                    (new_workload_1.name.clone(), new_workload_1),
                    (new_workload_2.name.clone(), new_workload_2),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().never();
        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .never();

        let mut server_state = ServerState {
            state: old_state.clone(),
            delete_graph: delete_graph_mock,
        };

        let result = server_state.update(rejected_new_state.clone(), vec![]);
        assert_eq!(
            result,
            Err(UpdateStateError::CycleInDependencies(
                "workload A".to_string()
            ))
        );

        // server state shall be the old state, new state shall be rejected
        assert_eq!(old_state, server_state.state);
    }

    // [utest->swdd~update-current-state-empty-update-mask~1]
    #[test]
    fn utest_server_state_update_state_replace_all_if_update_mask_empty() {
        let _ = env_logger::builder().is_test(true).try_init();
        let old_state = generate_test_old_state();
        let update_state = generate_test_update_state();
        let update_mask = vec![];

        let mut delete_graph_mock = MockDeleteGraph::new();

        delete_graph_mock.expect_insert().once().return_const(());

        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .once()
            .return_const(());

        let mut server_state = ServerState {
            state: old_state.clone(),
            delete_graph: delete_graph_mock,
        };

        server_state
            .update(update_state.clone(), update_mask)
            .unwrap();

        assert_eq!(update_state, server_state.state);
    }

    // [utest->swdd~update-current-state-with-update-mask~1]
    #[test]
    fn utest_server_state_update_state_replace_workload() {
        let _ = env_logger::builder().is_test(true).try_init();
        let old_state = generate_test_old_state();
        let update_state = generate_test_update_state();
        let update_mask = vec![format!("currentState.workloads.{}", WORKLOAD_NAME_1)];

        let new_workload = update_state
            .current_state
            .workloads
            .get(WORKLOAD_NAME_1)
            .unwrap()
            .clone();

        let mut expected = old_state.clone();
        expected
            .current_state
            .workloads
            .insert(new_workload.name.clone(), new_workload.clone());

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().once().return_const(());

        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .once()
            .return_const(());

        let mut server_state = ServerState {
            state: old_state.clone(),
            delete_graph: delete_graph_mock,
        };
        server_state.update(update_state, update_mask).unwrap();

        assert_eq!(expected, server_state.state);
    }

    // [utest->swdd~update-current-state-with-update-mask~1]
    #[test]
    fn utest_server_state_update_state_add_workload() {
        let old_state = generate_test_old_state();
        let update_state = generate_test_update_state();
        let update_mask = vec![format!("currentState.workloads.{}", WORKLOAD_NAME_4)];

        let new_workload = update_state
            .current_state
            .workloads
            .get(WORKLOAD_NAME_4)
            .unwrap()
            .clone();

        let mut expected = old_state.clone();
        expected
            .current_state
            .workloads
            .insert(WORKLOAD_NAME_4.into(), new_workload.clone());

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().once().return_const(());

        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .once()
            .return_const(());

        let mut server_state = ServerState {
            state: old_state.clone(),
            delete_graph: delete_graph_mock,
        };
        server_state.update(update_state, update_mask).unwrap();

        assert_eq!(expected, server_state.state);
    }

    // [utest->swdd~update-current-state-with-update-mask~1]
    #[test]
    fn utest_server_state_update_state_remove_workload() {
        let old_state = generate_test_old_state();
        let update_state = generate_test_update_state();
        let update_mask = vec![format!("currentState.workloads.{}", WORKLOAD_NAME_2)];

        let mut expected = old_state.clone();
        expected
            .current_state
            .workloads
            .remove(WORKLOAD_NAME_2)
            .unwrap();

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().once().return_const(());

        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .once()
            .return_const(());

        let mut server_state = ServerState {
            state: old_state.clone(),
            delete_graph: delete_graph_mock,
        };
        server_state.update(update_state, update_mask).unwrap();

        assert_eq!(expected, server_state.state);
    }

    // [utest->swdd~update-current-state-with-update-mask~1]
    #[test]
    fn utest_server_state_update_state_remove_non_existing_workload() {
        let old_state = generate_test_old_state();
        let update_state = generate_test_update_state();
        let update_mask = vec!["currentState.workloads.workload_5".into()];

        let expected = &old_state;

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().never();
        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .never();

        let mut server_state = ServerState {
            state: old_state.clone(),
            delete_graph: delete_graph_mock,
        };
        server_state.update(update_state, update_mask).unwrap();

        assert_eq!(*expected, server_state.state);
    }

    // [utest->swdd~update-current-state-with-update-mask~1]
    #[test]
    fn utest_server_state_update_state_remove_fails_from_non_map() {
        let old_state = generate_test_old_state();
        let update_state = generate_test_update_state();
        let update_mask = vec!["currentState.workloads.workload_2.tags.x".into()];

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().never();
        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .never();

        let mut server_state = ServerState {
            state: old_state.clone(),
            delete_graph: delete_graph_mock,
        };
        let result = server_state.update(update_state, update_mask);

        assert!(result.is_err());
        assert_eq!(server_state.state, old_state);
    }

    // [utest->swdd~update-current-state-with-update-mask~1]
    #[test]
    fn utest_server_state_update_state_fails_with_update_mask_empty_string() {
        let _ = env_logger::builder().is_test(true).try_init();
        let old_state = generate_test_old_state();
        let update_state = generate_test_update_state();
        let update_mask = vec!["".into()];

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().never();
        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .never();

        let mut server_state = ServerState {
            state: old_state.clone(),
            delete_graph: delete_graph_mock,
        };
        let result = server_state.update(update_state, update_mask);
        assert!(result.is_err());
        assert_eq!(server_state.state, old_state);
    }

    // [utest->swdd~update-current-state-empty-update-mask~1]
    #[test]
    fn utest_server_state_update_state_no_update() {
        let _ = env_logger::builder().is_test(true).try_init();

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().never();
        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .never();

        let mut server_state = ServerState {
            state: CompleteState::default(),
            delete_graph: delete_graph_mock,
        };

        let added_deleted_workloads = server_state
            .update(CompleteState::default(), vec![])
            .unwrap();
        assert!(added_deleted_workloads.is_none());
        assert_eq!(server_state.state, CompleteState::default());
    }

    // [utest->swdd~update-current-state-empty-update-mask~1]
    // [utest->swdd~server-detects-new-workload~1]
    #[test]
    fn utest_server_state_update_state_new_workloads() {
        let _ = env_logger::builder().is_test(true).try_init();

        let new_state = generate_test_update_state();
        let update_mask = vec![];

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().once().return_const(());

        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .once()
            .return_const(());

        let mut server_state = ServerState {
            state: CompleteState::default(),
            delete_graph: delete_graph_mock,
        };

        let added_deleted_workloads = server_state.update(new_state.clone(), update_mask).unwrap();
        assert!(added_deleted_workloads.is_some());

        let (mut added_workloads, deleted_workloads) = added_deleted_workloads.unwrap();
        added_workloads.sort_by(|left, right| left.name.cmp(&right.name));

        let mut expected_added_workloads: Vec<WorkloadSpec> = new_state
            .clone()
            .current_state
            .workloads
            .into_values()
            .collect();
        expected_added_workloads.sort_by(|left, right| left.name.cmp(&right.name));

        assert_eq!(added_workloads, expected_added_workloads);

        let expected_deleted_workloads: Vec<DeletedWorkload> = Vec::new();
        assert_eq!(deleted_workloads, expected_deleted_workloads);
        assert_eq!(server_state.state, new_state);
    }

    // [utest->swdd~update-current-state-empty-update-mask~1]
    // [utest->swdd~server-detects-deleted-workload~1]
    #[test]
    fn utest_server_state_update_state_deleted_workloads() {
        let _ = env_logger::builder().is_test(true).try_init();

        let current_complete_state = generate_test_old_state();
        let update_state = CompleteState::default();
        let update_mask = vec![];

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().once().return_const(());

        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .once()
            .return_const(());

        let mut server_state = ServerState {
            state: current_complete_state.clone(),
            delete_graph: delete_graph_mock,
        };

        let added_deleted_workloads = server_state.update(update_state, update_mask).unwrap();
        assert!(added_deleted_workloads.is_some());

        let (added_workloads, mut deleted_workloads) = added_deleted_workloads.unwrap();
        let expected_added_workloads: Vec<WorkloadSpec> = Vec::new();
        assert_eq!(added_workloads, expected_added_workloads);

        deleted_workloads.sort_by(|left, right| left.name.cmp(&right.name));
        let mut expected_deleted_workloads: Vec<DeletedWorkload> = current_complete_state
            .current_state
            .workloads
            .iter()
            .map(|(k, v)| DeletedWorkload {
                agent: v.agent.clone(),
                name: k.clone(),
                dependencies: HashMap::new(),
            })
            .collect();
        expected_deleted_workloads.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(deleted_workloads, expected_deleted_workloads);

        assert_eq!(server_state.state, CompleteState::default());
    }

    // [utest->swdd~update-current-state-empty-update-mask~1]
    // [utest->swdd~server-detects-changed-workload~1]
    #[test]
    fn utest_server_state_update_state_updated_workload() {
        let _ = env_logger::builder().is_test(true).try_init();

        let current_complete_state = generate_test_old_state();
        let mut new_complete_state = current_complete_state.clone();
        let update_mask = vec![];

        let workload_to_update = current_complete_state
            .current_state
            .workloads
            .get(WORKLOAD_NAME_1)
            .unwrap();

        let updated_workload = generate_test_workload_spec_with_param(
            AGENT_B.into(),
            workload_to_update.name.clone(),
            "runtime_2".into(),
        );
        new_complete_state
            .current_state
            .workloads
            .insert(workload_to_update.name.clone(), updated_workload.clone());

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock.expect_insert().once().return_const(());
        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .once()
            .return_const(());

        let mut server_state = ServerState {
            state: current_complete_state.clone(),
            delete_graph: delete_graph_mock,
        };

        let added_deleted_workloads = server_state
            .update(new_complete_state.clone(), update_mask)
            .unwrap();
        assert!(added_deleted_workloads.is_some());

        let (added_workloads, deleted_workloads) = added_deleted_workloads.unwrap();

        assert_eq!(added_workloads, vec![updated_workload]);

        assert_eq!(
            deleted_workloads,
            vec![DeletedWorkload {
                agent: workload_to_update.agent.clone(),
                name: workload_to_update.name.clone(),
                dependencies: HashMap::new(),
            }]
        );

        assert_eq!(server_state.state, new_complete_state);
    }

    // [utest->swdd~server-state-stores-delete-condition~1]
    // [utest->swdd~server-state-adds-delete-conditions-to-deleted-workload~1]
    #[test]
    fn utest_server_state_update_state_store_and_add_delete_conditions() {
        let _ = env_logger::builder().is_test(true).try_init();

        let workload = generate_test_workload_spec_with_param(
            AGENT_A.to_string(),
            WORKLOAD_NAME_1.to_string(),
            RUNTIME.to_string(),
        );

        let current_complete_state = CompleteState {
            current_state: State {
                workloads: HashMap::from([(workload.name.clone(), workload.clone())]),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut new_workload = workload.clone();
        new_workload.agent = AGENT_B.to_string();
        let new_complete_state = CompleteState {
            current_state: State {
                workloads: HashMap::from([(new_workload.name.clone(), new_workload.clone())]),
                ..Default::default()
            },
            ..Default::default()
        };

        let update_mask = vec![];

        let mut delete_graph_mock = MockDeleteGraph::new();
        delete_graph_mock
            .expect_insert()
            .with(mockall::predicate::eq(vec![new_workload]))
            .once()
            .return_const(());
        delete_graph_mock
            .expect_apply_delete_conditions_to()
            .with(mockall::predicate::eq(vec![DeletedWorkload {
                name: workload.name.clone(),
                agent: workload.agent.clone(),
                dependencies: HashMap::new(),
            }]))
            .once()
            .return_const(());

        let mut server_state = ServerState {
            state: current_complete_state,
            delete_graph: delete_graph_mock,
        };

        let added_deleted_workloads = server_state
            .update(new_complete_state.clone(), update_mask)
            .unwrap();
        assert!(added_deleted_workloads.is_some());
    }

    fn generate_test_old_state() -> CompleteState {
        generate_test_complete_state(vec![
            generate_test_workload_spec_with_param(
                "agent_A".into(),
                "workload_1".into(),
                "runtime_1".into(),
            ),
            generate_test_workload_spec_with_param(
                "agent_A".into(),
                "workload_2".into(),
                "runtime_2".into(),
            ),
            generate_test_workload_spec_with_param(
                "agent_B".into(),
                "workload_3".into(),
                "runtime_1".into(),
            ),
        ])
    }

    fn generate_test_update_state() -> CompleteState {
        generate_test_complete_state(vec![
            generate_test_workload_spec_with_param(
                "agent_B".into(),
                "workload_1".into(),
                "runtime_2".into(),
            ),
            generate_test_workload_spec_with_param(
                "agent_B".into(),
                "workload_3".into(),
                "runtime_2".into(),
            ),
            generate_test_workload_spec_with_param(
                "agent_A".into(),
                "workload_4".into(),
                "runtime_1".into(),
            ),
        ])
    }
}
