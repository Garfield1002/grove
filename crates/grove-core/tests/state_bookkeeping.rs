use grove_core::model::default_worktree_parent;
use grove_core::state::{AgentRecord, SessionRecord, State};

#[test]
fn public_bookkeeping_is_idempotent_through_the_production_library() {
    let mut state = State::default();

    state.ignore_session("scratch");
    state.ignore_session("scratch");
    assert!(state.is_ignored("scratch"));
    assert!(state.clear_ignored_sessions());
    assert!(!state.clear_ignored_sessions());

    assert!(state.assign_slot(4, "abc123"));
    assert_eq!(state.slot("abc123"), Some(4));
    assert_eq!(state.slot_worktree(4), Some("abc123"));
    assert!(state.clear_slot("abc123"));
    assert!(!state.clear_slot("abc123"));

    let agent = AgentRecord {
        worktree_id: "abc123".into(),
        session_id: "conversation-1".into(),
        transcript_path: "/tmp/transcript.jsonl".into(),
    };
    assert!(agent.has_transcript());
    assert!(state.record_agent(agent.clone()));
    assert!(!state.record_agent(agent));
    assert_eq!(
        state
            .agent("abc123")
            .map(|record| record.session_id.as_str()),
        Some("conversation-1")
    );
    assert!(state.forget_agent("abc123"));
    assert!(!state.forget_agent("abc123"));

    state.record_session(SessionRecord {
        worktree_id: "abc123".into(),
        project_id: "project-1".into(),
        worktree_path: "/tmp/tree".into(),
        session_name: "wt-abc123".into(),
        last_activity_at: 1,
    });
    assert_eq!(state.recorded_session_ids(), ["abc123"]);
    assert!(state.forget_session("abc123"));
    assert!(!state.forget_session("abc123"));

    assert_eq!(
        default_worktree_parent(None, std::path::Path::new("/src/grove")),
        std::path::Path::new("/src")
    );
    assert_eq!(
        default_worktree_parent(
            Some(std::path::Path::new("/trees")),
            std::path::Path::new("/src/grove"),
        ),
        std::path::Path::new("/trees")
    );
    state.normalize();
    assert!(!state.remove("missing-project"));
}
