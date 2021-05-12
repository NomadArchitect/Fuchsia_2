// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_zircon::{self as zx, AsHandleRef, HandleBased, Task as zxTask};
use log::warn;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::ops;
use std::sync::{Arc, Weak};

pub mod syscalls;

use crate::auth::Credentials;
use crate::fs::{FdTable, FileSystem};
use crate::mm::MemoryManager;
use crate::signals::types::*;
use crate::types::*;

// # Ownership structure
//
// The Kernel object is the root object of the task hierarchy.
//
// The Kernel owns the PidTable, which has the owning reference to each task in the
// kernel via its |tasks| field.
//
// Each task holds a reference to its ThreadGroup and an a reference to the objects
// for each major subsystem (e.g., file system, memory manager).
//
// # Back pointers
//
// Each ThreadGroup has weak pointers to its threads and to the kernel to which its
// threads belong.

pub struct Kernel {
    /// The Zircon job object that holds the processes running in this kernel.
    pub job: zx::Job,

    /// The processes and threads running in this kernel, organized by pid_t.
    pub pids: RwLock<PidTable>,
}

impl Kernel {
    pub fn new(name: &CString) -> Result<Arc<Kernel>, zx::Status> {
        let job = fuchsia_runtime::job_default().create_child_job()?;
        job.set_name(&name)?;
        let kernel = Kernel { job, pids: RwLock::new(PidTable::new()) };
        Ok(Arc::new(kernel))
    }
}

pub struct PidTable {
    /// The most-recently allocated pid in this table.
    last_pid: pid_t,

    /// The tasks in this table, organized by pid_t.
    ///
    /// This reference is the primary reference keeping the tasks alive.
    tasks: HashMap<pid_t, Weak<Task>>,
}

impl PidTable {
    pub fn new() -> PidTable {
        PidTable { last_pid: 0, tasks: HashMap::new() }
    }

    pub fn get_task(&self, pid: pid_t) -> Option<Arc<Task>> {
        self.tasks.get(&pid).and_then(|task| task.upgrade())
    }

    fn allocate_pid(&mut self) -> pid_t {
        self.last_pid += 1;
        return self.last_pid;
    }

    fn add_task(&mut self, task: &Arc<Task>) {
        assert!(!self.tasks.contains_key(&task.id));
        self.tasks.insert(task.id, Arc::downgrade(task));
    }

    fn remove_task(&mut self, pid: pid_t) {
        self.tasks.remove(&pid);
    }
}

pub struct ThreadGroup {
    /// The kernel to which this thread group belongs.
    pub kernel: Arc<Kernel>,

    /// A handle to the underlying Zircon process object.
    ///
    /// Currently, we have a 1-to-1 mapping between thread groups and zx::process
    /// objects. This approach might break down if/when we implement CLONE_VM
    /// without CLONE_THREAD because that creates a situation where two thread
    /// groups share an address space. To implement that situation, we might
    /// need to break the 1-to-1 mapping between thread groups and zx::process
    /// or teach zx::process to share address spaces.
    pub process: zx::Process,

    /// The lead task of this thread group.
    ///
    /// The lead task is typically the initial thread created in the thread group.
    pub leader: pid_t,

    /// The tasks in the thread group.
    pub tasks: RwLock<HashSet<pid_t>>,

    /// The signal actions that are registered for `tasks`. All `tasks` share the same `sigaction`
    /// for a given signal.
    pub signal_actions: RwLock<SignalActions>,
}

impl ThreadGroup {
    fn new(kernel: Arc<Kernel>, process: zx::Process, leader: pid_t) -> ThreadGroup {
        let mut tasks = HashSet::new();
        tasks.insert(leader);

        ThreadGroup {
            kernel,
            process,
            leader,
            tasks: RwLock::new(tasks),
            signal_actions: RwLock::new(SignalActions::default()),
        }
    }

    fn remove(&self, task: &Task) {
        let kill_process = {
            let mut tasks = self.tasks.write();
            self.kernel.pids.write().remove_task(task.id);
            tasks.remove(&task.id);
            tasks.is_empty()
        };
        if kill_process {
            if let Err(e) = self.process.kill() {
                warn!("Failed to kill process: {}", e);
            }
        }
    }
}

pub struct TaskOwner {
    pub task: Arc<Task>,
}

impl ops::Drop for TaskOwner {
    fn drop(&mut self) {
        self.task.destroy();
    }
}

pub struct Task {
    pub id: pid_t,

    /// The thread group to which this task belongs.
    pub thread_group: Arc<ThreadGroup>,

    /// The parent task, if any.
    pub parent: pid_t,

    // TODO: The children of this task.
    /// A handle to the underlying Zircon thread object.
    pub thread: zx::Thread,

    /// The file descriptor table for this task.
    pub files: Arc<FdTable>,

    /// The memory manager for this task.
    pub mm: Arc<MemoryManager>,

    /// The file system for this task.
    pub fs: Arc<FileSystem>,

    /// The security credentials for this task.
    pub creds: Credentials,

    // See https://man7.org/linux/man-pages/man2/set_tid_address.2.html
    pub set_child_tid: Mutex<UserAddress>,
    pub clear_child_tid: Mutex<UserAddress>,

    // See https://man7.org/linux/man-pages/man2/sigaltstack.2.html
    pub signal_stack: Mutex<Option<sigaltstack_t>>,

    /// The signal mask of the task.
    // See https://man7.org/linux/man-pages/man2/rt_sigprocmask.2.html
    pub signal_mask: Mutex<sigset_t>,
}

impl Task {
    pub fn new(
        kernel: &Arc<Kernel>,
        name: &CString,
        files: Arc<FdTable>,
        fs: Arc<FileSystem>,
        creds: Credentials,
    ) -> Result<TaskOwner, zx::Status> {
        let (process, root_vmar) = kernel.job.create_child_process(name.as_bytes())?;
        let thread = process.create_thread("initial-thread".as_bytes())?;

        // TODO: Stop giving MemoryManager a duplicate of the process handle once a process
        // handle is not needed to implement read_memory or write_memory.
        let duplicate_process = process.duplicate_handle(zx::Rights::SAME_RIGHTS)?;

        let mut pids = kernel.pids.write();
        let id = pids.allocate_pid();
        let task = Arc::new(Task {
            id,
            thread_group: Arc::new(ThreadGroup::new(kernel.clone(), process, id)),
            parent: 0,
            thread,
            files,
            mm: Arc::new(MemoryManager::new(duplicate_process, root_vmar)),
            fs,
            creds: creds,
            set_child_tid: Mutex::new(UserAddress::default()),
            clear_child_tid: Mutex::new(UserAddress::default()),
            signal_stack: Mutex::new(None),
            signal_mask: Mutex::new(sigset_t::default()),
        });
        pids.add_task(&task);

        Ok(TaskOwner { task })
    }

    /// Called by the Drop trait on TaskOwner.
    fn destroy(&self) {
        self.thread_group.remove(self);
    }

    pub fn get_task(&self, pid: pid_t) -> Option<Arc<Task>> {
        self.thread_group.kernel.pids.read().get_task(pid)
    }

    pub fn get_pid(&self) -> pid_t {
        self.thread_group.leader
    }

    pub fn get_tid(&self) -> pid_t {
        self.id
    }

    pub fn get_pgrp(&self) -> pid_t {
        // TODO: Implement process groups.
        1
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fuchsia_async as fasync;

    use crate::testing::*;

    #[fasync::run_singlethreaded(test)]
    async fn test_tid_allocation() {
        let (kernel, task_owner) = create_kernel_and_task();

        let task = &task_owner.task;
        assert_eq!(task.get_tid(), 1);
        let another_task_owner = Task::new(
            &kernel,
            &CString::new("another-task").unwrap(),
            FdTable::new(),
            task.fs.clone(),
            Credentials::default(),
        )
        .expect("failed to create second task");
        let another_task = &another_task_owner.task;
        assert_eq!(another_task.get_tid(), 2);

        let pids = kernel.pids.read();
        assert_eq!(pids.get_task(1).unwrap().get_tid(), 1);
        assert_eq!(pids.get_task(2).unwrap().get_tid(), 2);
        assert!(pids.get_task(3).is_none());
    }
}
