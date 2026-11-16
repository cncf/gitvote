use self::helpers::*;
use super::*;

mod check_runs;
mod collaborators;
mod comment_reactions;
mod comments;
mod config_file;
mod discussions;
mod event;
mod helpers;
mod installations;
mod labels;
mod module_functions;
mod pull_requests;
mod requests;

/// Marker looked up in the tests.
const MARKER: &str = "<!-- gitvote:vote-closed:00000000-0000-0000-0000-000000000001 -->";

/// Identifier of the test GitHub application.
const TEST_APP_ID: u64 = 1;
