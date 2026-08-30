use std::process::Child;

pub enum JobStatus {
    Running,
    #[allow(dead_code)]
    Done(i32),
}

pub struct Job {
    pub id: usize,
    pub command: String,
    pub child: Child,
    pub status: JobStatus,
}

pub struct JobTable {
    jobs: Vec<Job>,
    next_id: usize,
}

impl JobTable {
    pub fn new() -> Self {
        JobTable {
            jobs: Vec::new(),
            next_id: 1,
        }
    }

    /// Add a background job, return its job number.
    pub fn add(&mut self, command: String, child: Child) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        println!("[{}] {}", id, child.id());
        self.jobs.push(Job {
            id,
            command,
            child,
            status: JobStatus::Running,
        });
        id
    }

    /// Check for completed jobs, print notifications, and remove them.
    pub fn reap(&mut self) {
        let mut done = Vec::new();
        for job in &mut self.jobs {
            if let JobStatus::Running = job.status {
                match job.child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code().unwrap_or(-1);
                        job.status = JobStatus::Done(code);
                        done.push((job.id, job.command.clone(), code));
                    }
                    Ok(None) => {} // still running
                    Err(_) => {
                        job.status = JobStatus::Done(-1);
                        done.push((job.id, job.command.clone(), -1));
                    }
                }
            }
        }

        for (id, cmd, _code) in &done {
            println!("[{}] Done                    {}", id, cmd);
        }

        self.jobs.retain(|j| matches!(j.status, JobStatus::Running));
    }

    /// Print all jobs.
    pub fn list(&mut self) {
        self.reap();
        for job in &self.jobs {
            let status = match &job.status {
                JobStatus::Running => "Running",
                JobStatus::Done(_) => "Done",
            };
            println!("[{}] {:10} {}", job.id, status, job.command);
        }
    }

    /// Bring a job to the foreground and wait for it.
    pub fn fg(&mut self, id: Option<usize>) -> Option<i32> {
        let idx = if let Some(id) = id {
            self.jobs.iter().position(|j| j.id == id)
        } else {
            // Default to last job
            if self.jobs.is_empty() {
                None
            } else {
                Some(self.jobs.len() - 1)
            }
        };

        let idx = match idx {
            Some(i) => i,
            None => {
                eprintln!("fg: no such job");
                return None;
            }
        };

        let job = &mut self.jobs[idx];
        println!("{}", job.command);

        match job.child.wait() {
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                self.jobs.remove(idx);
                Some(code)
            }
            Err(e) => {
                eprintln!("fg: {}", e);
                self.jobs.remove(idx);
                None
            }
        }
    }
}
