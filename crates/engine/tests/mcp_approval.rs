#![cfg(windows)]
use domain::{Command, Request, ResponseData};
use engine::Engine;
use redb::{Database, ReadableTable, TableDefinition};
use std::{fs, path::PathBuf};

struct Fixture {
    root: PathBuf,
    db: PathBuf,
    run_id: String,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("magi-patch-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let worktree = root.join("worktree");
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(worktree.join("src")).unwrap();
        fs::write(worktree.join("src/file.txt"), "before").unwrap();
        let db = root.join("state.redb");
        let engine = Engine::open(&db).unwrap();
        let project = match call(
            &engine,
            Command::CreateProject {
                name: "fixture".into(),
                path: root.to_string_lossy().into_owned(),
            },
        )
        .unwrap()
        {
            ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        drop(engine);
        let database = Database::open(&db).unwrap();
        let tx = database.begin_write().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        {
            let mut projects = tx
                .open_table(TableDefinition::<&str, &str>::new("projects"))
                .unwrap();
            let mut value = serde_json::to_value(&project).unwrap();
            value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":"fixture","scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
            projects
                .insert(project.id.as_str(), value.to_string().as_str())
                .unwrap();
            let mut runs = tx
                .open_table(TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run = serde_json::json!({"id":run_id,"project_id":project.id,"spec_hash":project.draft_hash,"spec_version":1,"status":"awaiting_implementation","worktree":worktree,"scope":["src"],"events":[],"pending":null});
            runs.insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        drop(database);
        Self { root, db, run_id }
    }
    fn path(&self) -> PathBuf {
        self.root.join("worktree/src/file.txt")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db);
        let _ = fs::remove_file(self.path());
        let _ = fs::remove_dir(self.root.join("worktree/src"));
        let _ = fs::remove_dir(self.root.join("worktree"));
        let _ = fs::remove_dir(&self.root);
    }
}
fn call(engine: &Engine, command: Command) -> Result<ResponseData, String> {
    engine
        .handle(Request {
            id: "test".into(),
            command,
        })
        .result
}
#[tokio::test]
async fn mcp_request_requires_matching_explicit_approval_before_execution() {
    let fixture = Fixture::new();
    let engine = std::sync::Arc::new(Engine::open(&fixture.db).unwrap());
    let proposal = engine
        .clone()
        .handle_async(Request {
            id: "1".into(),
            command: Command::RequestMcpApproval {
                run_id: fixture.run_id.clone(),
                endpoint: "http://127.0.0.1:1/mcp".into(),
                name: "write".into(),
                arguments: serde_json::json!({"value":"x"}),
                cwd: ".".into(),
            },
        })
        .await
        .result
        .unwrap();
    let id = match proposal {
        ResponseData::Data(v) => v["approval_id"].as_str().unwrap().to_string(),
        _ => panic!("expected approval"),
    };
    let run = match call(
        &engine,
        Command::GetRun {
            run_id: fixture.run_id.clone(),
        },
    )
    .unwrap()
    {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(
        run["pending"]["arguments"]["endpoint"],
        "http://127.0.0.1:1/mcp"
    );
    assert_eq!(run["pending"]["arguments"]["arguments"]["value"], "x");
    assert!(engine
        .clone()
        .handle_async(Request {
            id: "2".into(),
            command: Command::CallMcpApproved {
                run_id: fixture.run_id.clone(),
                approval_id: id.clone()
            }
        })
        .await
        .result
        .is_err());
    assert!(call(
        &engine,
        Command::ApproveTool {
            run_id: fixture.run_id.clone(),
            approval_id: "wrong".into(),
            approved: true
        }
    )
    .is_err());
    call(
        &engine,
        Command::ApproveTool {
            run_id: fixture.run_id.clone(),
            approval_id: id.clone(),
            approved: true,
        },
    )
    .unwrap();
    assert!(engine
        .clone()
        .handle_async(Request {
            id: "3".into(),
            command: Command::CallMcpApproved {
                run_id: fixture.run_id.clone(),
                approval_id: id.clone()
            }
        })
        .await
        .result
        .is_err());
    assert!(engine
        .clone()
        .handle_async(Request {
            id: "4".into(),
            command: Command::CallMcpApproved {
                run_id: fixture.run_id.clone(),
                approval_id: id
            }
        })
        .await
        .result
        .is_err());
}

#[tokio::test]
async fn approved_mcp_calls_bound_tool_once_and_never_replays_unknown_outcome() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for succeeds in [true, false] {
        let fixture = Fixture::new();
        let engine = std::sync::Arc::new(Engine::open(&fixture.db).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for step in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                loop {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length: ")
                                    .and_then(|v| v.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if data.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(data).unwrap();
                if step == 3 {
                    let v: serde_json::Value =
                        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    assert_eq!(v["method"], "tools/call");
                    assert_eq!(v["params"]["name"], "write");
                    assert_eq!(
                        v["params"]["arguments"],
                        serde_json::json!({"value":"approved"})
                    );
                    if !succeeds {
                        return;
                    }
                }
                let body = match step {
                    0 => r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
                    1 => "",
                    2 => {
                        r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"write","inputSchema":{"type":"object","required":["value"]}}]}}"#
                    }
                    _ => {
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"done"}]}}"#
                    }
                };
                socket.write_all(format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",if step==1 {"202 Accepted"} else {"200 OK"},body.len(),body).as_bytes()).await.unwrap();
            }
        });
        let response = engine
            .clone()
            .handle_async(Request {
                id: "1".into(),
                command: Command::RequestMcpApproval {
                    run_id: fixture.run_id.clone(),
                    endpoint: format!("http://{addr}/mcp"),
                    name: "write".into(),
                    arguments: serde_json::json!({"value":"approved"}),
                    cwd: ".".into(),
                },
            })
            .await
            .result
            .unwrap();
        let id = match response {
            ResponseData::Data(v) => v["approval_id"].as_str().unwrap().to_owned(),
            _ => panic!("approval"),
        };
        call(
            &engine,
            Command::ApproveTool {
                run_id: fixture.run_id.clone(),
                approval_id: id.clone(),
                approved: true,
            },
        )
        .unwrap();
        let invoke = || Request {
            id: "2".into(),
            command: Command::CallMcpApproved {
                run_id: fixture.run_id.clone(),
                approval_id: id.clone(),
            },
        };
        let result = engine.clone().handle_async(invoke()).await.result;
        assert_eq!(result.is_ok(), succeeds, "{result:?}");
        server.await.unwrap();
        assert!(engine.clone().handle_async(invoke()).await.result.is_err());
        let status = match call(
            &engine,
            Command::GetRun {
                run_id: fixture.run_id.clone(),
            },
        )
        .unwrap()
        {
            ResponseData::Data(v) => v["status"].as_str().unwrap().to_string(),
            _ => panic!("status"),
        };
        assert_eq!(
            status,
            if succeeds {
                "awaiting_implementation"
            } else {
                "uncertain"
            }
        );
        drop(engine);
        let reopened = std::sync::Arc::new(Engine::open(&fixture.db).unwrap());
        assert!(reopened.handle_async(invoke()).await.result.is_err());
    }
}

#[tokio::test]
async fn cancelling_during_discovery_prevents_call_and_status_remains_responsive() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = Fixture::new();
    let engine = std::sync::Arc::new(Engine::open(&fixture.db).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut ready = Some(ready_tx);
        let mut release = Some(release_rx);
        for step in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut b = [0; 4096];
            socket.read(&mut b).await.unwrap();
            if step == 2 {
                ready.take().unwrap().send(()).unwrap();
                release.take().unwrap().await.unwrap();
            }
            let body = match step {
                0 => r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
                1 => "",
                _ => {
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"write","inputSchema":{"type":"object"}}]}}"#
                }
            };
            socket.write_all(format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",if step==1 {"202 Accepted"}else{"200 OK"},body.len(),body).as_bytes()).await.unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
            .await
            .is_err()
    });
    let response = engine
        .clone()
        .handle_async(Request {
            id: "1".into(),
            command: Command::RequestMcpApproval {
                run_id: fixture.run_id.clone(),
                endpoint: format!("http://{addr}/mcp"),
                name: "write".into(),
                arguments: serde_json::json!({}),
                cwd: ".".into(),
            },
        })
        .await
        .result
        .unwrap();
    let id = match response {
        ResponseData::Data(v) => v["approval_id"].as_str().unwrap().to_string(),
        _ => panic!("approval"),
    };
    call(
        &engine,
        Command::ApproveTool {
            run_id: fixture.run_id.clone(),
            approval_id: id.clone(),
            approved: true,
        },
    )
    .unwrap();
    let invoke = tokio::spawn(engine.clone().handle_async(Request {
        id: "2".into(),
        command: Command::CallMcpApproved {
            run_id: fixture.run_id.clone(),
            approval_id: id.clone(),
        },
    }));
    ready_rx.await.unwrap();
    assert!(engine
        .clone()
        .handle_async(Request {
            id: "duplicate".into(),
            command: Command::CallMcpApproved {
                run_id: fixture.run_id.clone(),
                approval_id: id.clone()
            }
        })
        .await
        .result
        .is_err());
    assert!(tokio::time::timeout(
        std::time::Duration::from_secs(2),
        engine.clone().handle_async(Request {
            id: "3".into(),
            command: Command::Status
        })
    )
    .await
    .unwrap()
    .result
    .is_ok());
    engine
        .clone()
        .handle_async(Request {
            id: "4".into(),
            command: Command::CancelRun {
                run_id: fixture.run_id.clone(),
            },
        })
        .await
        .result
        .unwrap();
    release_tx.send(()).unwrap();
    assert!(server.await.unwrap(), "cancelled call reached server");
    assert!(invoke.await.unwrap().result.is_err());
}

#[tokio::test]
async fn denied_or_cancelled_approval_cannot_trigger_mcp_request() {
    for deny in [true, false] {
        let fixture = Fixture::new();
        let engine = std::sync::Arc::new(Engine::open(&fixture.db).unwrap());
        let response = engine
            .clone()
            .handle_async(Request {
                id: "1".into(),
                command: Command::RequestMcpApproval {
                    run_id: fixture.run_id.clone(),
                    endpoint: "http://127.0.0.1:1/mcp".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    cwd: ".".into(),
                },
            })
            .await
            .result
            .unwrap();
        let id = match response {
            ResponseData::Data(v) => v["approval_id"].as_str().unwrap().to_owned(),
            _ => panic!("approval"),
        };
        if deny {
            call(
                &engine,
                Command::ApproveTool {
                    run_id: fixture.run_id.clone(),
                    approval_id: id.clone(),
                    approved: false,
                },
            )
            .unwrap();
        } else {
            call(
                &engine,
                Command::ApproveTool {
                    run_id: fixture.run_id.clone(),
                    approval_id: id.clone(),
                    approved: true,
                },
            )
            .unwrap();
            call(
                &engine,
                Command::CancelRun {
                    run_id: fixture.run_id.clone(),
                },
            )
            .unwrap();
        }
        let result = engine
            .clone()
            .handle_async(Request {
                id: "2".into(),
                command: Command::CallMcpApproved {
                    run_id: fixture.run_id.clone(),
                    approval_id: id,
                },
            })
            .await
            .result;
        assert!(result.is_err());
    }
}

#[tokio::test]
async fn changed_specification_invalidates_recorded_approval() {
    let fixture = Fixture::new();
    let engine = std::sync::Arc::new(Engine::open(&fixture.db).unwrap());
    let proposal = engine
        .clone()
        .handle_async(Request {
            id: "1".into(),
            command: Command::RequestMcpApproval {
                run_id: fixture.run_id.clone(),
                endpoint: "http://127.0.0.1:1/mcp".into(),
                name: "write".into(),
                arguments: serde_json::json!({}),
                cwd: ".".into(),
            },
        })
        .await
        .result
        .unwrap();
    let id = match proposal {
        ResponseData::Data(v) => v["approval_id"].as_str().unwrap().to_owned(),
        _ => panic!("approval"),
    };
    call(
        &engine,
        Command::ApproveTool {
            run_id: fixture.run_id.clone(),
            approval_id: id.clone(),
            approved: true,
        },
    )
    .unwrap();
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let runs = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let row = runs.get(fixture.run_id.as_str()).unwrap().unwrap();
        let run: serde_json::Value = serde_json::from_str(row.value()).unwrap();
        let project_id = run["project_id"].as_str().unwrap();
        let mut projects = tx
            .open_table(TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut project: serde_json::Value = {
            let row = projects.get(project_id).unwrap().unwrap();
            serde_json::from_str(row.value()).unwrap()
        };
        project["draft_hash"] = "changed".into();
        projects
            .insert(project_id, project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = std::sync::Arc::new(Engine::open(&fixture.db).unwrap());
    let err = engine
        .handle_async(Request {
            id: "2".into(),
            command: Command::CallMcpApproved {
                run_id: fixture.run_id.clone(),
                approval_id: id,
            },
        })
        .await
        .result
        .unwrap_err();
    assert!(err.contains("approved specification changed"), "{err}");
}

#[tokio::test]
async fn interrupted_mcp_execution_is_uncertain_after_restart() {
    let fixture = Fixture::new();
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut runs = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let mut run: serde_json::Value = {
            let row = runs.get(fixture.run_id.as_str()).unwrap().unwrap();
            serde_json::from_str(row.value()).unwrap()
        };
        run["status"] = "executing".into();
        run["pending"] = serde_json::json!({"id":"approved-id","tool":"mcp","authorized":true,"arguments":{"endpoint":"http://127.0.0.1:1/mcp","name":"write","arguments":{},"cwd":"."}});
        runs.insert(fixture.run_id.as_str(), run.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = std::sync::Arc::new(Engine::open(&fixture.db).unwrap());
    let run = match call(
        &engine,
        Command::GetRun {
            run_id: fixture.run_id.clone(),
        },
    )
    .unwrap()
    {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "uncertain");
    assert!(engine
        .handle_async(Request {
            id: "1".into(),
            command: Command::CallMcpApproved {
                run_id: fixture.run_id.clone(),
                approval_id: "approved-id".into()
            }
        })
        .await
        .result
        .is_err());
}
