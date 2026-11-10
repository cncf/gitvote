use self::helpers::*;
use super::*;

mod cancel_vote;
mod check_vote;
mod close_finished_vote;
mod close_vote;
mod commands_handler;
mod create_vote;
mod db_contracts;
mod helpers;
mod module_functions;
mod process_command;
mod status_checker;
mod votes_auto_closer;
mod votes_closer;
mod votes_processor;

/// Maximum time to wait for a running worker to reach the point being tested.
const WORKER_TIMEOUT: Duration = Duration::from_secs(5);
