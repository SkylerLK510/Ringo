//! Offline differential coverage for whole-task accounting. Fixtures include paid failures, duplicates,
//! unsettled usage, aggregate scopes, malformed counters, and counts beyond signed 64-bit integers.
use ringo_cli::rt::{py::Num, pyjson, taskreport};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixtures() -> Vec<Value> {
    let mut cases = vec![
        json!({"records":[]}),
        json!({"records":[],"summary":{"calls":2,"input":50,"output":5},"final_state":"done"}),
        json!({"records":[null,"invalid",[],{"status":"ok","usage":{"input":1}}]}),
        json!({"records":[{"step":"s","attempt":1,"status":"ok","usage":{"input":1,"models":["a","b"],"warnings":["private"]}}]}),
        json!({"records":[{"step":"s","attempt":1,"status":"ok","started":2,"usage":{"input":5,"total_input_tokens":500}},
                          {"step":"s","attempt":2,"status":"ok","started":1,"usage":{"input":5,"total_input_tokens":300}}]}),
        json!({"records":[{"status":"error"}],"blockers":[{"signature":"error:a","count":4},{"signature":"error:a","count":2}],"summary":{"calls":2,"unfinished_calls":1,"unpriced_calls":2,"elapsed_s":2.5},"max_tokens":5,"overshoot_calls":1}),
        json!({"records":[{"status":"ok","usage":{"input":18446744073709551616u128,"context_tokens":18446744073709551617u128}},{"status":"ok","context_tokens":18446744073709551618u128,"usage":{"input":1}}],"final_state":"ready_for_review"}),
        json!({"records":[{"status":"ok","compacted":true,"usage":{"input":5}},{"status":"ok","usage":{"input":5,"compaction_count":1}}]}),
        json!({"records":[{"status":null,"usage":{"input":4}}]}),
        json!({"records":[{"usage":{"input":99}}, {"status":"ok","usage":{"input":5}}]}),
        json!({"records":[{"status":"error"}, {"status":"fatal","stopped":"timeout"}]}),
        json!({"records":[{"status":"ok","usage":{"context_window":1000000,"compaction_count":0,"compacted":false}},
                          {"status":"ok","usage":{"compact_threshold":800000,"context_compacted":false}}]}),
        json!({"records":[],"blockers":[{}, {"signature":null}, {"signature":"None","count":18446744073709551616u128}, {"signature":1,"count":2.5}, {"signature":true,"count":-2}]}),
        json!({"records":[],"blockers":[{"count":null}]}),
        json!({"records":[],"blockers":[{"signature":[]}]}),
        // the run's cost-weighted usage: copied whole when it is an object, left out when it is anything else
        json!({"records":[{"step":"s","attempt":1,"status":"ok","usage":{"input":5,"surface":"cli:codex","billing":"subscription","batch":false}}],
               "summary":{"calls":1,"weighted":{"unit":"codex_credits","value":0.25,"unknown_models":[],"unweighted_calls":0,"by_unit":{"codex_credits":0.25}}},"final_state":"done"}),
        json!({"records":[],"summary":{"calls":2,"weighted":{"unit":"mixed","value":null,"unknown_models":["gpt-7"],"unweighted_calls":1,
               "by_unit":{"codex_credits":1.5,"claude_weighted_tokens":null}}},"final_state":"done"}),
        json!({"records":[],"summary":{"calls":1,"weighted":5},"final_state":"done"}),
    ];
    for status in ["ok", "error", "malformed", "fatal", "started", "admission_refused"] {
        for usage in [
            json!({"input":100,"cache_write":5,"cache_read":50,"output":10}),
            json!({"input":5}), json!({}), json!({"input":-1,"output":true}),
            json!({"input":1.5,"cache_read":"10","output":3}),
            json!({"input":null,"output":0,"context_tokens":500}),
        ] {
            let rec = json!({"step":"s","attempt":1,"status":status,"started":1,"usage":usage,"stopped":"timeout"});
            cases.push(json!({"records":[rec.clone(),rec],"summary":{"untokened_calls":99},"final_state":"failed"}));
        }
    }
    cases
}

#[test]
fn reports_match_python_on_accounting_edge_cases() {
    let cases = fixtures();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut child = Command::new(ringo_cli::python()).args(["-c", r#"
import json, sys
sys.path.insert(0, sys.argv[1])
import taskreport
for line in sys.stdin:
    case = json.loads(line)
    try:
        result = {'ok': taskreport.build(**case)}
    except Exception as exc:
        result = {'err': [type(exc).__name__, str(exc)]}
    print(json.dumps(result), flush=True)
"#]).arg(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let input = cases.iter().map(|v| format!("{}\n", pyjson::dumps(v))).collect::<String>();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), cases.len());
    for (i, (case, expected)) in cases.iter().zip(lines).enumerate() {
        let result = taskreport::build(
            case["records"].as_array().unwrap(), case.get("summary").and_then(Value::as_object),
            case.get("blockers").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]),
            case.get("final_state").and_then(Value::as_str).unwrap_or("unknown"), None,
            case.get("max_tokens").and_then(Num::from_value), case.get("overshoot_calls").and_then(Value::as_i64),
            None, None, None,
        );
        let actual = match result { Ok(v) => json!({"ok":v}), Err(e) => json!({"err":[e.exc.name(), e.msg]}) };
        assert_eq!(pyjson::dumps(&actual), expected, "fixture {i}: {}", pyjson::dumps(case));
    }
}
