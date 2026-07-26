use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

#[tokio::test]
async fn convert_discovers_the_project_and_writes_a_valid_draft() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"model": "test-converter"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": "{\"name\":\"nested-draft\",\"quality\":\"medium\",\"sections\":[{\"id\":\"prompt\",\"priority\":100,\"body\":\"ignored\"}]}"
                }
            }]
        })))
        .mount(&server)
        .await;

    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();
    let nested = project.path().join("a/b");
    std::fs::create_dir_all(&nested).unwrap();

    cybersin()
        .current_dir(&nested)
        .env("OPENROUTER_API_KEY", "test-key")
        .env("OPENROUTER_BASE_URL", server.uri())
        .arg("convert")
        .arg("--model")
        .arg("test-converter")
        .arg("Write from a nested directory.")
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote "))
        .stdout(predicate::str::contains("self-validation passed"));

    let written = project.path().join("prompts/nested-draft.prompt.yaml");
    assert!(written.exists());
    assert!(std::fs::read_to_string(written)
        .unwrap()
        .contains("Write from a nested directory."));
}

#[test]
fn convert_reports_a_missing_project_with_a_nonzero_exit() {
    let outside_project = tempfile::tempdir().unwrap();

    cybersin()
        .current_dir(outside_project.path())
        .arg("convert")
        .arg("A raw prompt")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cybersin.yaml"));
}

#[test]
fn convert_reports_missing_openrouter_key() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();

    cybersin()
        .current_dir(project.path())
        .env_remove("OPENROUTER_API_KEY")
        .arg("convert")
        .arg("A raw prompt")
        .assert()
        .failure()
        .stderr(predicate::str::contains("OPENROUTER_API_KEY"));
}

#[tokio::test]
async fn convert_validation_failure_is_nonzero_and_keeps_the_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": "{\"name\":\"needs-review\",\"quality\":\"invalid\",\"sections\":[{\"id\":\"prompt\",\"priority\":100,\"body\":\"ignored\"}]}"
                }
            }]
        })))
        .mount(&server)
        .await;

    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();

    cybersin()
        .current_dir(project.path())
        .env("OPENROUTER_API_KEY", "test-key")
        .env("OPENROUTER_BASE_URL", server.uri())
        .arg("convert")
        .arg("Keep this draft for review.")
        .assert()
        .failure()
        .stderr(predicate::str::contains("self-validation failed"))
        .stderr(predicate::str::contains("needs-review.prompt.yaml"));

    assert!(project
        .path()
        .join("prompts/needs-review.prompt.yaml")
        .exists());
}

#[tokio::test]
async fn convert_invalid_output_contract_schema_is_nonzero_and_writes_no_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": "{\"name\":\"invalid-contract\",\"quality\":\"medium\",\"sections\":[{\"id\":\"prompt\",\"priority\":100,\"body\":\"ignored\"}],\"output_contract\":{\"type\":\"json_schema\",\"schema\":\"not valid json\"}}"
                }
            }]
        })))
        .mount(&server)
        .await;

    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();

    cybersin()
        .current_dir(project.path())
        .env("OPENROUTER_API_KEY", "test-key")
        .env("OPENROUTER_BASE_URL", server.uri())
        .arg("convert")
        .arg("Return JSON with a summary field.")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "invalid output_contract.schema JSON",
        ));

    assert!(!project
        .path()
        .join("prompts/invalid-contract.prompt.yaml")
        .exists());
}
