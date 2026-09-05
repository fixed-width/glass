//! Offline schema reporting matches real stdio advertisements without launching an app.

use std::process::Stdio;
use std::time::Duration;

use rmcp::ServiceExt;

#[tokio::test]
async fn offline_report_matches_stdio_for_both_profiles() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for profile in ["full", "lean"] {
            let directory = tempfile::tempdir().unwrap();
            let audit_path = directory.path().join("audit.jsonl");
            let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_glass-mcp"))
                .args(["--tool-profile", profile, "tools", "--json", "--audit-log"])
                .arg(&audit_path)
                .env("XDG_CACHE_HOME", directory.path())
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(output.status.success(), "{:?}", output.stderr);
            assert_eq!(
                std::fs::read_dir(directory.path()).unwrap().count(),
                0,
                "offline reporting must not open the audit sink or artifact store"
            );
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["profile"], profile);
            assert_eq!(
                report["tools_json_bytes"],
                serde_json::to_vec(&report["tools"]).unwrap().len()
            );
            assert_eq!(
                report["instructions_bytes"],
                report["instructions"].as_str().unwrap().len()
            );

            let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_glass-mcp"))
                .args(["--tool-profile", profile])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let transport = (
                process.stdout.take().unwrap(),
                process.stdin.take().unwrap(),
            );
            let client = ().serve(transport).await.unwrap();
            let tools = client.list_all_tools().await.unwrap();
            assert_eq!(serde_json::to_value(tools).unwrap(), report["tools"]);
            assert_eq!(
                client.peer_info().unwrap().instructions.as_deref(),
                report["instructions"].as_str()
            );
            client.cancel().await.unwrap();
            let status = process.wait().await.unwrap();
            assert!(status.success(), "stdio server failed: {status}");
        }
    })
    .await
    .expect("bounded stdio profile check");
}
