//! [Scheduler] module for executing a series of tasks for a given
//! [Workflow].
//!
//! [Scheduler]: TaskScheduler

use crate::{
    db::{Connection, Database},
    workflow::{Resource, Vertex},
    Db, Workflow,
};
use anyhow::Result;
use dagga::Node;
use futures::future::BoxFuture;
use homestar_core::workflow::{InstructionResult, LinkMap, Pointer};
use homestar_wasm::io::Arg;
use indexmap::IndexMap;
use libipld::Cid;
use std::{ops::ControlFlow, str::FromStr};

type Schedule<'a> = Vec<Vec<Node<Vertex<'a>, usize>>>;

/// Type for [instruction]-based, batched, execution graph and set of task
/// resources.
///
/// [instruction]: homestar_core::workflow::Instruction
#[allow(missing_debug_implementations)]
pub struct ExecutionGraph<'a> {
    /// A built-up [Dag] [Schedule] of batches.
    ///
    /// [Dag]: dagga::Dag
    pub(crate) schedule: Schedule<'a>,
    /// Vector of [resources] to fetch for executing functions in [Workflow].
    ///
    /// [resources]: Resource
    pub(crate) resources: Vec<Resource>,
}

/// Scheduler for a series of tasks, including what's run,
/// what's left to run, and data structures to track resources
/// and what's been executed in memory.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct TaskScheduler<'a> {
    /// In-memory map of task/instruction results.
    pub(crate) linkmap: LinkMap<InstructionResult<Arg>>,
    /// [ExecutionGraph] of what's been run so far for a [Workflow] of `batched`
    /// [Tasks].
    ///
    /// [Tasks]: homestar_core::workflow::Task
    pub(crate) ran: Option<Schedule<'a>>,

    /// [ExecutionGraph] of what's left to run for a [Workflow] of `batched`
    /// [Tasks].
    ///
    /// [Tasks]: homestar_core::workflow::Task
    pub(crate) run: Schedule<'a>,

    /// Step/batch to resume from.
    pub(crate) resume_step: Option<usize>,

    /// Resources that tasks within a [Workflow] rely on, retrieved
    /// through IPFS Client or the DHT directly ahead-of-time.
    ///
    /// This is transferred from the [ExecutionGraph] for actually executing the
    /// schedule.
    pub(crate) resources: IndexMap<Resource, Vec<u8>>,
}

impl<'a> TaskScheduler<'a> {
    /// Initialize Task Scheduler, given [Receipt] cache.
    ///
    /// [Receipt]: crate::Receipt
    pub async fn init<F>(
        workflow: Workflow<'a, Arg>,
        conn: &mut Connection,
        fetch_fn: F,
    ) -> Result<TaskScheduler<'a>>
    where
        F: FnOnce(Vec<Resource>) -> BoxFuture<'a, Result<IndexMap<Resource, Vec<u8>>>>,
    {
        let graph = workflow.graph()?;
        let mut schedule = graph.schedule;
        let schedule_length = schedule.len();
        let fetched = fetch_fn(graph.resources).await?;

        let resume = schedule
            .iter()
            .enumerate()
            .rev()
            .try_for_each(|(idx, vec)| {
                let folded_pointers = vec.iter().try_fold(vec![], |mut ptrs, node| {
                    ptrs.push(Pointer::new(Cid::from_str(node.name())?));
                    Ok::<_, anyhow::Error>(ptrs)
                });

                if let Ok(pointers) = folded_pointers {
                    match Db::find_instructions(pointers, conn) {
                        Ok(found) => {
                            let linkmap = found.iter().fold(
                                LinkMap::<InstructionResult<Arg>>::new(),
                                |mut map, receipt| {
                                    if let Ok(cid) = receipt.instruction().try_into() {
                                        let _ = map.insert(cid, receipt.output_as_arg());
                                    }
                                    map
                                },
                            );

                            if found.len() == vec.len() {
                                ControlFlow::Break((idx + 1, linkmap))
                            } else if !found.is_empty() && found.len() < vec.len() {
                                ControlFlow::Break((idx, linkmap))
                            } else {
                                ControlFlow::Continue(())
                            }
                        }
                        _ => ControlFlow::Continue(()),
                    }
                } else {
                    ControlFlow::Continue(())
                }
            });

        match resume {
            ControlFlow::Break((idx, linkmap)) => {
                let pivot = schedule.split_off(idx);
                let step = if idx >= schedule_length || idx == 0 {
                    None
                } else {
                    Some(idx)
                };

                Ok(Self {
                    linkmap,
                    ran: Some(schedule),
                    run: pivot,
                    resume_step: step,
                    resources: fetched,
                })
            }
            _ => Ok(Self {
                linkmap: LinkMap::<InstructionResult<Arg>>::new(),
                ran: None,
                run: schedule,
                resume_step: None,
                resources: fetched,
            }),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{db::Database, settings::Settings, test_utils, Receipt};
    use futures::FutureExt;
    use homestar_core::{
        test_utils::workflow as workflow_test_utils,
        workflow::{
            config::Resources, instruction::RunInstruction, prf::UcanPrf, Invocation,
            Receipt as InvocationReceipt, Task,
        },
    };
    use libipld::Ipld;

    #[tokio::test]
    async fn initialize_task_scheduler() {
        let config = Resources::default();
        let (instruction1, instruction2, _) =
            workflow_test_utils::related_wasm_instructions::<Arg>();
        let task1 = Task::new(
            RunInstruction::Expanded(instruction1),
            config.clone().into(),
            UcanPrf::default(),
        );
        let task2 = Task::new(
            RunInstruction::Expanded(instruction2),
            config.into(),
            UcanPrf::default(),
        );

        let db = test_utils::db::MemoryDb::setup_connection_pool(Settings::load().unwrap().node())
            .unwrap();
        let mut conn = db.conn().unwrap();
        let workflow = Workflow::new(vec![task1.clone(), task2.clone()]);
        let fetch_fn = |_rscs: Vec<Resource>| { async { Ok(IndexMap::default()) } }.boxed();
        let scheduler = TaskScheduler::init(workflow, &mut conn, fetch_fn)
            .await
            .unwrap();

        assert!(scheduler.linkmap.is_empty());
        assert!(scheduler.ran.is_none());
        assert_eq!(scheduler.run.len(), 2);
        assert_eq!(scheduler.resume_step, None);
    }

    #[tokio::test]
    async fn initialize_task_scheduler_with_receipted_instruction() {
        let config = Resources::default();
        let (instruction1, instruction2, _) =
            workflow_test_utils::related_wasm_instructions::<Arg>();
        let task1 = Task::new(
            RunInstruction::Expanded(instruction1.clone()),
            config.clone().into(),
            UcanPrf::default(),
        );
        let task2 = Task::new(
            RunInstruction::Expanded(instruction2),
            config.into(),
            UcanPrf::default(),
        );

        let invocation_receipt = InvocationReceipt::new(
            Invocation::new(task1.clone()).try_into().unwrap(),
            InstructionResult::Ok(Ipld::Integer(4)),
            Ipld::Null,
            None,
            UcanPrf::default(),
        );
        let receipt = Receipt::try_with(
            instruction1.clone().try_into().unwrap(),
            &invocation_receipt,
        )
        .unwrap();

        let db = test_utils::db::MemoryDb::setup_connection_pool(Settings::load().unwrap().node())
            .unwrap();
        let mut conn = db.conn().unwrap();

        let stored_receipt =
            test_utils::db::MemoryDb::store_receipt(receipt.clone(), &mut conn).unwrap();

        assert_eq!(receipt, stored_receipt);

        let workflow = Workflow::new(vec![task1.clone(), task2.clone()]);
        let fetch_fn = |_rscs: Vec<Resource>| { async { Ok(IndexMap::default()) } }.boxed();
        let scheduler = TaskScheduler::init(workflow, &mut conn, fetch_fn)
            .await
            .unwrap();
        let ran = scheduler.ran.as_ref().unwrap();

        assert_eq!(scheduler.linkmap.len(), 1);
        assert!(scheduler
            .linkmap
            .contains_key(&Cid::try_from(instruction1).unwrap()));
        assert_eq!(ran.len(), 1);
        assert_eq!(scheduler.run.len(), 1);
        assert_eq!(scheduler.resume_step, Some(1));
    }

    #[tokio::test]
    async fn initialize_task_scheduler_with_all_receipted_instruction() {
        let config = Resources::default();
        let (instruction1, instruction2, _) =
            workflow_test_utils::related_wasm_instructions::<Arg>();

        let task1 = Task::new(
            RunInstruction::Expanded(instruction1.clone()),
            config.clone().into(),
            UcanPrf::default(),
        );

        let task2 = Task::new(
            RunInstruction::Expanded(instruction2.clone()),
            config.into(),
            UcanPrf::default(),
        );

        let invocation_receipt1 = InvocationReceipt::new(
            Invocation::new(task1.clone()).try_into().unwrap(),
            InstructionResult::Ok(Ipld::Integer(4)),
            Ipld::Null,
            None,
            UcanPrf::default(),
        );
        let receipt1 = Receipt::try_with(
            instruction1.clone().try_into().unwrap(),
            &invocation_receipt1,
        )
        .unwrap();

        let invocation_receipt2 = InvocationReceipt::new(
            Invocation::new(task2.clone()).try_into().unwrap(),
            InstructionResult::Ok(Ipld::Integer(44)),
            Ipld::Null,
            None,
            UcanPrf::default(),
        );
        let receipt2 = Receipt::try_with(
            instruction2.clone().try_into().unwrap(),
            &invocation_receipt2,
        )
        .unwrap();

        let db = test_utils::db::MemoryDb::setup_connection_pool(Settings::load().unwrap().node())
            .unwrap();
        let mut conn = db.conn().unwrap();

        let rows_inserted =
            test_utils::db::MemoryDb::store_receipts(vec![receipt1, receipt2], &mut conn).unwrap();

        assert_eq!(2, rows_inserted);

        let workflow = Workflow::new(vec![task1.clone(), task2.clone()]);
        let fetch_fn = |_rscs: Vec<Resource>| { async { Ok(IndexMap::default()) } }.boxed();
        let scheduler = TaskScheduler::init(workflow, &mut conn, fetch_fn)
            .await
            .unwrap();
        let ran = scheduler.ran.as_ref().unwrap();

        assert_eq!(scheduler.linkmap.len(), 1);
        assert!(!scheduler
            .linkmap
            .contains_key(&Cid::try_from(instruction1).unwrap()),);
        assert!(scheduler
            .linkmap
            .contains_key(&Cid::try_from(instruction2).unwrap()));
        assert_eq!(ran.len(), 2);
        assert!(scheduler.run.is_empty());
        assert_eq!(scheduler.resume_step, None);
    }
}
