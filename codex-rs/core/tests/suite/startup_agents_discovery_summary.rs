use codex_core::features::Feature;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_output_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_agents_summary_prepends_discovery_section() {
    let server = start_mock_server().await;
    let startup_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("startup-summary"),
            ev_output_text_delta(
                r#"{"summaries":[{"path":"AGENTS.md","why":"defines repository rules","when":"before making changes in this workspace"}]}"#,
            ),
            ev_completed("startup-summary"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::StartupAgentsDiscoverySummary)
            .expect("enable startup agents discovery summary");
        std::fs::write(config.cwd.join("AGENTS.md"), "follow project rules")
            .expect("write AGENTS.md");
    });
    let test = builder.build(&server).await.expect("build test codex");

    let startup_request = startup_mock.single_request();
    assert!(startup_request.body_contains_text("\"path\": \"AGENTS.md\""));
    assert!(startup_request.body_contains_text("follow project rules"));

    let turn_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("turn"), ev_completed("turn")]),
    )
    .await;

    test.submit_turn("hello").await.expect("submit turn");

    let request = turn_mock.single_request();
    let user_messages = request.message_input_texts("user");
    let instructions = user_messages
        .iter()
        .find(|text| text.contains("## Startup AGENTS discovery tree (gitignore-aware)"))
        .unwrap_or_else(|| {
            panic!("missing startup discovery section in messages: {user_messages:#?}")
        });

    assert!(
        instructions.contains("AGENTS.md"),
        "expected AGENTS entry in discovery tree: {instructions}"
    );
    assert!(
        instructions.contains("why: defines repository rules"),
        "expected why summary in discovery tree: {instructions}"
    );
    assert!(
        instructions.contains("when: before making changes in this workspace"),
        "expected when summary in discovery tree: {instructions}"
    );
    assert!(
        instructions.contains("follow project rules"),
        "expected project doc contents preserved: {instructions}"
    );

    let tree_pos = instructions
        .find("## Startup AGENTS discovery tree (gitignore-aware)")
        .expect("startup section marker");
    let project_doc_contents_pos = instructions
        .find("follow project rules")
        .expect("project doc contents");
    assert!(
        tree_pos < project_doc_contents_pos,
        "expected startup tree prepended before project doc contents: {instructions}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_agents_summary_invalid_payload_falls_back_without_section() {
    let server = start_mock_server().await;
    let startup_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("startup-summary"),
            ev_output_text_delta(
                r#"{"summaries":[{"path":"OTHER.md","why":"wrong file","when":"never"}]}"#,
            ),
            ev_completed("startup-summary"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::StartupAgentsDiscoverySummary)
            .expect("enable startup agents discovery summary");
        std::fs::write(config.cwd.join("AGENTS.md"), "follow project rules")
            .expect("write AGENTS.md");
    });

    let test = builder.build(&server).await.expect("build test codex");
    assert_eq!(startup_mock.requests().len(), 1);

    let turn_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("turn"), ev_completed("turn")]),
    )
    .await;

    test.submit_turn("hello").await.expect("submit turn");

    let request = turn_mock.single_request();
    let user_messages = request.message_input_texts("user");
    assert!(
        user_messages
            .iter()
            .all(|text| !text.contains("## Startup AGENTS discovery tree (gitignore-aware)")),
        "expected startup discovery section to be omitted after invalid summary: {user_messages:#?}"
    );
}
