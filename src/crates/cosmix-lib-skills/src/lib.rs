pub mod domain;
mod indexd_client;
mod llm_client;
mod loop_fns;
mod prompts;
mod types;

pub use domain::{detect_domain, detect_domain_cwd};
pub use indexd_client::{IndexdClient, StatsResponse, SourceCount, StaleResponse, StaleChunk};
pub use llm_client::LlmClient;
pub use loop_fns::{check_graduation, evaluate_task, extract_skill, format_skills_for_prompt, refine_skill, retrieve_skills, retrieve_skills_domain};
pub use types::{EvalScore, Message, SkillDocument, TaskOutcome, TaskTranscript, ToolCall};
