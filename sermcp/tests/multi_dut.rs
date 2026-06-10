//! End-to-end multi-DUT routing against loopback simulators and real binaries.

use std::io::{BufRead, Write};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Board {
    port: u16,
    received: Arc<Mutex<Vec<u8>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Board {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Board {
    async fn start(name: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let capture = received.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut tick = tokio::time::interval(Duration::from_millis(80));
                let mut input = [0; 4096];
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            let output = format!("\r\n{name}-tick\r\nroot@{name}:/# ");
                            if stream.write_all(output.as_bytes()).await.is_err() { break; }
                        }
                        read = stream.read(&mut input) => match read {
                            Ok(0) | Err(_) => break,
                            Ok(n) => capture.lock().unwrap().extend_from_slice(&input[..n]),
                        }
                    }
                }
            }
        });
        Self {
            port,
            received,
            task,
        }
    }
}

struct Client {
    http: reqwest::Client,
    base: String,
    session: String,
}

fn frame(body: &str) -> Value {
    if let Ok(value) = serde_json::from_str(body) {
        return value;
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|value| value.get("id").is_some())
        .unwrap_or_else(|| panic!("No response frame: {body}"))
}

impl Client {
    async fn connect(base: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let response = http
            .post(format!("{base}/mcp"))
            .header("Accept", "application/json, text/event-stream")
            .json(
                &json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                    "protocolVersion":"2025-06-18", "capabilities":{},
                    "clientInfo":{"name":"dutabo", "version":"test"}
                }}),
            )
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let session = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(frame(&response.text().await.unwrap())["result"]["serverInfo"].is_object());
        let client = Self {
            http,
            base,
            session,
        };
        let response = client
            .http
            .post(format!("{}/mcp", client.base))
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-session-id", &client.session)
            .header("mcp-protocol-version", "2025-06-18")
            .json(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        client
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self
            .http
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-session-id", &self.session)
            .header("mcp-protocol-version", "2025-06-18")
            .json(&json!({"jsonrpc":"2.0", "id":2, "method":method, "params":params}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        frame(&response.text().await.unwrap())
    }

    async fn tool(&self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({"name":name, "arguments":arguments}))
            .await
    }
}

fn payload(response: &Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn stdio_response(
    reader: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    id: u64,
) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = reader.recv().await.expect("stdio server closed");
            if response["id"] == id {
                return response;
            }
        }
    })
    .await
    .expect("stdio request timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_process_routes_two_boards_and_keeps_logging_when_one_disconnects() {
    // Exercise both entry transports. Everything belongs to a scratch project.
    for http_mode in [false, true] {
        let project = tempfile::tempdir().unwrap();
        let board_a = Board::start("board-a").await;
        let board_b = Board::start("board-b").await;
        let config = project.path().join(".target.jsonc");
        std::fs::write(&config, json!({
            "lock_dir":project.path().join("locks"),
            "dev_hosts":[{"ip":"127.0.0.1", "duts":[
                {"dut_name":"board-a", "serial":{"port":board_a.port,"baudrate":115200}, "control":{"dev_ctrl":"none"}},
                {"dut_name":"board-b", "serial":{"port":board_b.port,"baudrate":115200}, "control":{"dev_ctrl":"none"}}
            ]}]
        }).to_string()).unwrap();
        let port_guard = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let explicit_port = port_guard.local_addr().unwrap().port();
        drop(port_guard);
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_sermcp"));
        command
            .current_dir(project.path())
            .env("TARGET_CONF", &config)
            // A legacy selection must not prevent the other board starting.
            .env("TARGET_DUT_NAME", "board-a")
            .env("SERMCP_STARTUP_PROBE_TIMEOUT_MS", "500")
            .env_remove("SERMCP_SESSION_PID")
            .env_remove("SERMCP_SESSION_START_TICKS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if http_mode {
            command.args(["--http", &format!("127.0.0.1:{explicit_port}")]);
        }
        let mut server = Server(command.spawn().unwrap());
        let (stdio_tx, mut stdio_rx) = tokio::sync::mpsc::unbounded_channel();
        let stdout = server.0.stdout.take().unwrap();
        let stdout_reader = tokio::task::spawn_blocking(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(value) = serde_json::from_str(&line)
                    && stdio_tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let inventory_path = project.path().join(".dut-serial/inventory.json");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let base = loop {
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "server exited early"
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "multi-DUT server did not start"
            );
            let port = std::fs::read(&inventory_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|inventory| inventory["mcp_http_port"].as_u64());
            if let Some(port) = port {
                let base = format!("http://127.0.0.1:{port}");
                if let Ok(response) = http.get(format!("{base}/health")).send().await
                    && let Ok(health) = response.json::<Value>().await
                    && health["duts"].as_array().is_some_and(|rows| {
                        rows.len() == 2 && rows.iter().all(|row| row["serial"]["owned"] == true)
                    })
                {
                    break base;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        let client = Client::connect(base.clone()).await;
        if !http_mode {
            let stdin = server.0.stdin.as_mut().unwrap();
            writeln!(stdin, "{}", json!({"jsonrpc":"2.0", "id":31, "method":"initialize", "params":{
                "protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"agent-test", "version":"1"}
            }})).unwrap();
            assert!(stdio_response(&mut stdio_rx, 31).await["result"]["serverInfo"].is_object());
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
            )
            .unwrap();
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc":"2.0", "id":32, "method":"tools/call", "params":{
                    "name":"serial_get_state", "arguments":{"dut":"board-b"}
                }})
            )
            .unwrap();
            assert_eq!(
                payload(&stdio_response(&mut stdio_rx, 32).await)["dut_name"],
                "board-b"
            );
        }
        let tools = client.rpc("tools/list", json!({})).await;
        let state_tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "serial_get_state")
            .unwrap();
        assert!(
            state_tool["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("dut"))
        );
        let listed = payload(&client.tool("serial_list_duts", json!({})).await);
        assert_eq!(listed["managed_duts"], json!(["board-a", "board-b"]));
        assert!(listed["current_dut"].is_null());
        for name in ["board-a", "board-b"] {
            let state = payload(&client.tool("serial_get_state", json!({"dut":name})).await);
            assert_eq!(state["dut_name"], name);
            assert!(state["log_path"].as_str().unwrap().contains(name));
            let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_dutabo"))
                .current_dir(project.path())
                .env("TARGET_CONF", &config)
                .args(["-d", name, "status"])
                .output()
                .await
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains(&format!("/{name}/logs/")));
        }
        for args in [json!({}), json!({"dut":"unknown"}), json!({"dut":false})] {
            let rejected = client.tool("serial_send_raw", args).await;
            assert!(rejected["error"].is_object(), "{rejected}");
        }
        let ws_base = base.replacen("http://", "ws://", 1);
        assert!(
            tokio_tungstenite::connect_async(format!("{ws_base}/serial/ws"))
                .await
                .is_err()
        );
        let (mut manual_a, _) =
            tokio_tungstenite::connect_async(format!("{ws_base}/serial/ws?dut=board-a"))
                .await
                .unwrap();
        // Wait for the upgrade worker to claim the selected engine.
        for _ in 0..50 {
            if payload(
                &client
                    .tool("serial_get_state", json!({"dut":"board-a"}))
                    .await,
            )["state"]
                == "dutabo"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            payload(
                &client
                    .tool("serial_get_state", json!({"dut":"board-a"}))
                    .await
            )["state"],
            "dutabo"
        );
        assert_eq!(
            payload(&client.tool("serial_list_duts", json!({})).await)["dut_count"],
            2
        );
        assert_ne!(
            payload(
                &client
                    .tool("serial_get_state", json!({"dut":"board-b"}))
                    .await
            )["state"],
            "dutabo"
        );
        let sent = client
            .tool(
                "serial_send_raw",
                json!({"dut":"board-b", "data":"only-board-b\n"}),
            )
            .await;
        assert_ne!(sent["result"]["isError"], true, "{sent}");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            String::from_utf8_lossy(&board_b.received.lock().unwrap()).contains("only-board-b")
        );
        assert!(
            !String::from_utf8_lossy(&board_a.received.lock().unwrap()).contains("only-board-b")
        );
        manual_a
            .send(tokio_tungstenite::tungstenite::Message::Close(None))
            .await
            .unwrap();
        drop(manual_a);

        let (mut monitor_b, _) = tokio_tungstenite::connect_async(format!(
            "{ws_base}/serial/ws?dut=board-b&mode=monitor"
        ))
        .await
        .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(3), monitor_b.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&message.into_data()).contains("board-b"));
        drop(monitor_b);
        let resources = client.rpc("resources/list", json!({})).await;
        assert!(
            resources["result"]["resources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|resource| resource["uri"] == "log://serial/full?dut=board-b")
        );
        let resource = client
            .rpc(
                "resources/read",
                json!({"uri":"log://serial/full?dut=board-b"}),
            )
            .await;
        assert_eq!(
            resource["result"]["contents"][0]["uri"],
            "log://serial/full?dut=board-b"
        );
        let log = resource["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(log.contains("board-b-tick"));
        assert!(!log.contains("board-a-tick"));

        let log_b = project
            .path()
            .join(".dut-serial/board-b/logs/full.serial.log");
        let before = std::fs::metadata(&log_b).unwrap().len();
        drop(board_a);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            std::fs::metadata(&log_b).unwrap().len() > before,
            "board B stopped logging when A disconnected"
        );
        let health: Value = http
            .get(format!("{base}/health?dut=board-b"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["serial"]["owned"], true);
        assert_eq!(health["dut_name"], "board-b");
        assert!(server.0.try_wait().unwrap().is_none());
        // No process-wide takeover may disturb B or kill the singleton.
        unsafe {
            libc::kill(server.0.id() as i32, libc::SIGUSR1);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(server.0.try_wait().unwrap().is_none());
        drop(client);
        if !http_mode {
            drop(server.0.stdin.take());
            let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
            while server.0.try_wait().unwrap().is_none() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "stdio shutdown left engines alive"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!project.path().join(".dut-serial/board-b/mcp.pid").exists());
        }
        drop(server);
        let _ = stdout_reader.await;
    }
}
