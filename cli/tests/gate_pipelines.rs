//! Differential gate for `rt::pipelines` against `pipelines.py`: each case runs through the Python module and the
//! Rust port on the same input; results are compared byte for byte (`json.dumps` on the Python side, `pyjson::dumps`
//! here). Offline, no model calls. Corpus sweeps over the repository and its git history are `#[ignore]`d.
#![cfg(unix)]

use ringo_cli::rt::pipelines::{self as pl, NeedVerdicts, Replies};
use ringo_cli::rt::{Error, pyjson};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DRIVER: &str = r#"
import json, sys
sys.path.insert(0, sys.argv[1])
import pipelines as pl

def cat(e):
    if isinstance(e, pl.NeedVerdicts):
        return 'NeedVerdicts'
    for name, t in (('ValueError', ValueError), ('OSError', OSError), ('TypeError', (TypeError, AttributeError)),
                    ('KeyError', KeyError)):
        if isinstance(e, t):
            return name
    return type(e).__name__

RX = {'HEAD': pl.HEAD, 'SECTION': pl.SECTION, 'VERDICT': pl.VERDICT, 'CUT': pl.CUT}

def run(op, a):
    if op == 'findings': return list(pl.findings(a[0], a[1]))
    if op == 'unfenced': return list(pl.unfenced(a[0]))
    if op == 'check_reply': return pl.check_reply(a[0], a[0]['passes'][a[1]], a[2])
    if op == 'has_issues': return pl.has_issues(a[0])
    if op == 'verdicts': return list(pl.verdicts(a[0]))
    if op == 'scope_verdicts': return pl.scope_verdicts(a[0], a[1])
    if op == 'all_findings': return pl.all_findings(a[0], a[1])
    if op == 'instruction': return pl.instruction(a[0], a[0]['passes'][a[1]], a[2])
    if op == 'requests': return pl.requests(a[0], a[0]['passes'][a[1]], a[2])
    if op == 'combine': return pl.combine(a[0], a[0]['passes'][a[1]], a[2], a[3])
    if op == 'repair_request': return pl.repair_request(a[0], a[0]['passes'][a[1]], a[2], pl.NeedVerdicts(a[3], a[4]))
    if op == 'assemble': return list(pl.assemble(a[0], a[1]))
    if op == 'waves': return [[p['id'] for p in w] for w in pl.waves(a[0])]
    if op == 'validate': return pl.validate(a[0], a[1])
    if op == 'load_plan': return pl.load_plan(a[0], a[1])
    if op == 'finditer': return [[m.start(), m.end(), *m.groups()] for m in RX[a[0]].finditer(a[1])]
    if op == 'finding_id':
        m = pl.FINDING_ID.search(a[0])
        return m and m.group(0)
    if op == 'no_defects': return bool(pl.NO_DEFECTS.search(a[0]))
    if op == 'head_match':
        m = pl.HEAD.match(a[0])
        return m and [m.end(), *m.groups()]
    if op == 'need':
        e = pl.NeedVerdicts(a[0], a[1])
        return [str(e), e.ids, e.reason]
    if op == 'consts':
        return [pl.REPLIES, sorted(pl.PASS_KEYS), sorted(pl.PLAN_KEYS), pl.MAX_FINDINGS, pl.ID, str(pl.PROMPTS), str(pl.PLANS)]
    raise SystemExit('unknown op ' + op)

out = sys.stdout
for op, args in json.load(sys.stdin):
    try:
        r = {'ok': run(op, args)}
    except Exception as e:
        r = {'err': cat(e), 'msg': str(e)}
        if isinstance(e, pl.NeedVerdicts):
            r['ids'], r['reason'] = e.ids, e.reason
    out.write(json.dumps(r) + '\n')
"#;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Each case's `json.dumps` line from the Python module.
fn python(cases: &[(String, Vec<Value>)]) -> Vec<String> {
    let input: Vec<Value> = cases.iter().map(|(op, args)| json!([op, args])).collect();
    let mut child = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(DRIVER)
        .arg(root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 runs");
    let text = pyjson::dumps(&Value::Array(input));
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(text.as_bytes()));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<String> = String::from_utf8(out.stdout).unwrap().lines().map(str::to_string).collect();
    assert_eq!(lines.len(), cases.len());
    lines
}

fn err_json(e: &Error) -> Value {
    let mut m = Map::new();
    m.insert("err".into(), json!(e.exc.name()));
    m.insert("msg".into(), json!(e.msg));
    if let Some(n) = NeedVerdicts::from_error(e) {
        m.insert("ids".into(), json!(n.ids));
        m.insert("reason".into(), json!(n.reason));
    }
    Value::Object(m)
}

fn ok(v: Value) -> Value {
    json!({"ok": v})
}

fn st(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("a string: {v}"))
}

fn replies(v: &Value) -> Replies {
    v.as_object().unwrap().iter().map(|(k, v)| (k.clone(), st(v).to_string())).collect()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|s| st(s).to_string()).collect()
}

fn map<V: Into<Value> + Clone>(m: &indexmap::IndexMap<String, V>) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect())
}

fn ci(text: &str, b: usize) -> usize {
    text[..b].chars().count()
}

fn nth_pass(plan: &Value, i: &Value) -> Value {
    plan["passes"][i.as_u64().unwrap() as usize].clone()
}

fn rust(op: &str, a: &[Value]) -> Value {
    let r: Result<Value, Error> = match op {
        "findings" => {
            let (pre, found, sections) = pl::findings(st(&a[0]), st(&a[1]));
            Ok(json!([pre, map(&found), map(&sections)]))
        }
        "unfenced" => Ok(json!(pl::unfenced(st(&a[0])))),
        "check_reply" => pl::check_reply(&a[0], &nth_pass(&a[0], &a[1]), st(&a[2])).map(|()| Value::Null),
        "has_issues" => Ok(json!(pl::has_issues(st(&a[0])))),
        "verdicts" => {
            let (v, cuts) = pl::verdicts(st(&a[0]));
            let v: Map<String, Value> = v.iter().map(|(k, (v, t))| (k.clone(), json!([v, t]))).collect();
            Ok(json!([v, map(&cuts)]))
        }
        "scope_verdicts" => Ok(json!(pl::scope_verdicts(st(&a[0]), &strings(&a[1])))),
        "all_findings" => Ok(map(&pl::all_findings(&a[0], &replies(&a[1])))),
        "instruction" => Ok(json!(pl::instruction(&a[0], &nth_pass(&a[0], &a[1]), &replies(&a[2])))),
        "requests" => Ok(json!(pl::requests(&a[0], &nth_pass(&a[0], &a[1]), &replies(&a[2])))),
        "combine" => Ok(json!(pl::combine(&a[0], &nth_pass(&a[0], &a[1]), &replies(&a[2]), &strings(&a[3])))),
        "repair_request" => {
            let need = NeedVerdicts::new(strings(&a[3]), st(&a[4]));
            Ok(json!(pl::repair_request(&a[0], &nth_pass(&a[0], &a[1]), &replies(&a[2]), &need)))
        }
        "assemble" => pl::assemble(&a[0], &replies(&a[1])).map(|(text, stats)| json!([text, stats])),
        "waves" => pl::waves(&a[0]).map(|w| {
            json!(w.iter().map(|wave| wave.iter().map(|p| p["id"].clone()).collect::<Vec<_>>()).collect::<Vec<_>>())
        }),
        "validate" => pl::validate(a[0].clone(), Path::new(st(&a[1]))),
        "load_plan" => {
            let path = a[0].as_str().map(Path::new);
            pl::load_plan(&root(), path, st(&a[1]))
        }
        "finditer" => {
            let rx = match st(&a[0]) {
                "HEAD" => &*pl::HEAD,
                "SECTION" => &*pl::SECTION,
                "VERDICT" => &*pl::VERDICT,
                _ => &*pl::CUT,
            };
            let text = st(&a[1]);
            let out: Vec<Value> = rx
                .captures_iter(text)
                .map(|c| {
                    let m = c.get(0).unwrap();
                    let mut row = vec![json!(ci(text, m.start())), json!(ci(text, m.end()))];
                    row.extend(c.iter().skip(1).map(|g| json!(g.map(|g| g.as_str()))));
                    Value::Array(row)
                })
                .collect();
            Ok(Value::Array(out))
        }
        "finding_id" => Ok(json!(pl::finding_id(st(&a[0])))),
        "no_defects" => Ok(json!(pl::no_defects(st(&a[0])))),
        "head_match" => {
            let text = st(&a[0]);
            Ok(match pl::head_match(text) {
                Some(c) => json!([ci(text, c.get(0).unwrap().end()), &c[1], &c[2]]),
                None => Value::Null,
            })
        }
        "need" => {
            let n = NeedVerdicts::new(strings(&a[0]), st(&a[1]));
            Ok(json!([n.error().msg, n.ids, n.reason]))
        }
        "consts" => {
            let mut pass_keys = pl::PASS_KEYS.to_vec();
            pass_keys.sort();
            let mut plan_keys = pl::PLAN_KEYS.to_vec();
            plan_keys.sort();
            Ok(json!([
                pl::REPLIES,
                pass_keys,
                plan_keys,
                pl::MAX_FINDINGS,
                r"[A-Za-z][\w-]*",
                pl::prompts_dir(&root()).to_string_lossy(),
                pl::plans_dir(&root()).to_string_lossy()
            ]))
        }
        _ => panic!("unknown op {op}"),
    };
    match r {
        Ok(v) => ok(v),
        Err(e) => err_json(&e),
    }
}

/// Runs every case on both sides; returns the Python lines (for coverage checks) and panics on any mismatch.
fn gate(name: &str, cases: &[(String, Vec<Value>)]) -> Vec<String> {
    let want = python(cases);
    let mut bad = Vec::new();
    for ((op, args), py) in cases.iter().zip(&want) {
        let got = pyjson::dumps(&rust(op, args));
        if &got != py {
            bad.push(format!("{op} {}\n  python: {py}\n  rust:   {got}", pyjson::dumps(&Value::Array(args.clone()))));
        }
    }
    eprintln!("{name}: {} cases, {} mismatches", cases.len(), bad.len());
    assert!(bad.is_empty(), "{name}: {} of {} cases differ:\n{}", bad.len(), cases.len(), bad[..bad.len().min(8)].join("\n"));
    want
}

fn case(op: &str, args: Vec<Value>) -> (String, Vec<Value>) {
    (op.to_string(), args)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

const NUMBERS: &[&str] = &["1", "2", "3", "10", "0", "07", "\u{661}", "\u{663}", "\u{b9}", "12", "\u{ff11}"];
const TITLES: &[&str] = &[
    "Bug one", "Bug two (`a.py:2`)", "Side claim here.", "`lsmysql.New` panics on nil", "Unicode \u{e9} \u{4e2d}\u{6587} \u{1f600}",
    "", "Open questions", "Not checked", "No defects found", "KEEP", "a.1 KEEP", "**Bold** title**", " spaced ",
];
const HEADINGS: &[&str] = &[
    "### {n}. {t}", "## {n}) {t}", "#### {n}: {t}", "### **{n}. {t}**", "### F{n}. {t}", "### Ab{n}. {t}",
    "### abc{n}. {t}", "###{n}.{t}", "### {n} . {t}", "##   **  {n} .  {t}", "# {n}. {t}", "##### {n}. {t}",
    " ### {n}. {t}", "### {n}.", "###\t{n}.\t{t}", "##\n{n}. {t}", "### \u{a0}{n}. {t}", "### \u{1c}{n}. {t}",
    "### {n}){t}", "### {n}:", "### **{n}.** {t}", "### Finding {n}. {t}", "### {n}. {t} (`a.py:{n}`)",
    "####{n}) {t}\r", "### {n}.\n\n{t}", "## \u{2028}{n}. {t}", "### *{n}*. {t}", "###  {n}.  {t}  ",
];
const SECTIONS: &[&str] = &[
    "### Open questions", "## Not checked", "#### Not checked for null", "#### Open questions about ownership",
    "### **Open questions**", "### Open questions (not findings)", "### Not checked:", "### NOT CHECKED",
    "### Open quest\u{130}ons", "### open quest\u{131}ons", "### Open que\u{17f}tions", "### Not chec\u{212a}ed",
    "### Open questions (a) (b)", "###Open questions", "##  ** Open questions  **  :  ", "# Open questions",
    "##### Not checked", "### Not checked\r", "### Open questions (x)\t:", "### Not  checked", "#### Open questions",
    "## Open Questions", "### Not checked (runtime)", "### Open questions:", "### ** Not checked ** :",
];
const FENCES: &[&str] = &[
    "```", "```python", "~~~~", "````", "   ```", "    ```", "```\r", "~~~", "~~~~~", "`````markdown", "``", "\t```",
    "~~~~markdown", "```` ", "  ~~~",
];
const BODY: &[&str] = &[
    "- Problem: p1. Side claim here.", "Side claim here.", "- Code: `a  b`", "text \u{e9} \u{4e2d}\u{6587} \u{1f600} a\u{301}",
    "No defects found.", "**No defects found**", "no defects foundation", "NO DEFECTS FOUND here", "- none", "none",
    "None.", "-", "", "  ", "\t", "Evidence.", "  - Nested detail stays.", "Explanation.", "\u{2028}sep", "\u{1c}",
    "e", "- Fix: hold the lock.", "Body mentions a.1 and b.2.", "NONE", "NONE - no issues", "**NONE**", "`NONE`.",
    "nOnE: x", "NONE\u{2014} y", "NONE \u{2014} dash", "- Is it deployed?", "runtime", "- `n.py:3`", "x  Side claim here.  y",
    "No defects found\u{301}", "\u{2014} no defects found", "1. Numbered list item", "What this change does: x.",
    "Summary.", "q.py:1", "    indented code ### 1. not", "\u{feff}bom", "x\u{0}y", "\u{85}nel",
];
const IDS: &[&str] = &[
    "a.1", "a.2", "a.3", "b.1", "b.2", "k.1", "ghost.1", "review.12", "k-x.3", "a.10", "a_b.1", "\u{e9}.1", "A.1", "r.1",
];
const VERDICT_LINES: &[&str] = &[
    "{a} KEEP - q", "{a}: **KEEP** - yes", "{a} REFUTED - wrong", "{a} DUP {b} - same", "{a} `DUP` of **{b}**",
    "{a} OPEN - needs unseen code", "{a} CUT \"{s}\"", "{a} **CUT** \"{s}\"", "- {a} KEEPER", "{a}KEEP", "{a} keep",
    "{a} KEEP_x", "{a} KEEP2", "{a} KEEP\u{301}", "{a} DUP x_{b}", "{a} DUP \u{e9}{b}", "{a} DUP", "{a} DUP {b}{b}",
    "* [{a}] - KEEP: fine", "{a} - CUT - \"{s}\" -", "{a} CUT \"{s}\" trailing", "{a} CUT \"\"", "{a} CUT \"a \"b\" c\"",
    "{a}\tOPEN", "{a} OPEN\r", "1. {a} KEEP", "{a} DUP {b}\r", "{a} \u{2014} REFUTED", "{a} KEEP \u{e9}", "\u{e9}{a} KEEP",
    "{a}.5 KEEP", "{a} CUT \"{s}\"\r", "**{a}** CUT: \"{s}\"",
];
const SENTENCES: &[&str] = &["Side claim here.", "e", "p1.", "Problem:", "a  b", "x", "Evidence.", "- Fix:", "lock."];

fn fill(form: &str, rng: &mut Rng) -> String {
    let mut out = form.to_string();
    for key in ["{n}", "{t}", "{a}", "{b}", "{s}"] {
        while let Some(i) = out.find(key) {
            let v = match key {
                "{n}" => *rng.pick(NUMBERS),
                "{t}" => *rng.pick(TITLES),
                "{a}" | "{b}" => *rng.pick(IDS),
                _ => *rng.pick(SENTENCES),
            };
            out.replace_range(i..i + key.len(), v);
        }
    }
    out
}

fn line(rng: &mut Rng) -> String {
    match rng.below(20) {
        0..=4 => fill(rng.pick(HEADINGS), rng),
        5..=6 => rng.pick(SECTIONS).to_string(),
        7..=8 => rng.pick(FENCES).to_string(),
        9..=10 => fill(rng.pick(VERDICT_LINES), rng),
        _ => rng.pick(BODY).to_string(),
    }
}

fn wrap(text: String, rng: &mut Rng) -> String {
    let crlf = rng.chance(10);
    let mut text = if crlf { text.replace('\n', "\r\n") } else { text };
    match rng.below(14) {
        0 => text = format!("```markdown\n{text}\n```"),
        1 => text = format!("`````\n{text}\n`````"),
        2 => text = format!("~~~~\n{text}\n~~~~~"),
        3 => text = format!("```\n{text}"),
        4 => text = format!("  \n\t{text}\n \u{a0}"),
        _ => {}
    }
    text
}

fn text(rng: &mut Rng) -> String {
    let n = rng.below(26);
    let lines: Vec<String> = (0..n).map(|_| line(rng)).collect();
    wrap(lines.join("\n"), rng)
}

/// A reviewer's reply: a preamble, findings with bodies and fences, maybe sections.
fn review(rng: &mut Rng, findings: usize) -> String {
    let mut lines = Vec::new();
    if rng.chance(70) {
        lines.push(rng.pick(&["What this change does: x.", "Summary.", "No defects found.", "Intro\nNo defects found."]).to_string());
        lines.push(String::new());
    }
    for _ in 0..findings {
        let form = if rng.chance(80) { "### {n}. {t}" } else { *rng.pick(HEADINGS) };
        lines.push(fill(form, rng));
        for _ in 0..rng.below(5) {
            lines.push(if rng.chance(15) { rng.pick(FENCES).to_string() } else { rng.pick(BODY).to_string() });
        }
        if rng.chance(20) {
            lines.extend(["```python".into(), "# Side claim here.".into(), "### 9. quoted".into(), "```".into()]);
        }
        lines.push(String::new());
    }
    if findings == 0 && rng.chance(60) {
        lines.push("No defects found.".into());
    }
    for _ in 0..rng.below(3) {
        lines.push(rng.pick(SECTIONS).to_string());
        for _ in 0..rng.below(3) {
            lines.push(rng.pick(&["- Is it deployed?", "- none", "none", "runtime", "- `n.py:3`", "### 1. Numbered question", "", "- runtime"]).to_string());
        }
    }
    wrap(lines.join("\n"), rng)
}

/// A checker reply for the given finding ids and bodies.
fn checker(rng: &mut Rng, items: &[(String, String)]) -> String {
    let mut lines = Vec::new();
    for (fid, body) in items {
        let deco = |rng: &mut Rng, v: &str| -> String {
            match rng.below(6) {
                0 => format!("{fid}: **{v}**"),
                1 => format!("- {fid} `{v}`"),
                2 => format!("[{fid}] - {v}:"),
                _ => format!("{fid} {v}"),
            }
        };
        match rng.below(20) {
            0..=8 => lines.push(deco(rng, "KEEP") + " - evidence"),
            9..=11 => lines.push(deco(rng, "REFUTED")),
            12..=14 => {
                let target = if rng.chance(70) { rng.pick(items).0.clone() } else { rng.pick(IDS).to_string() };
                lines.push(format!("{} {target}", deco(rng, "DUP")));
            }
            15..=16 => lines.push(deco(rng, "OPEN") + " - needs unseen code"),
            17 => lines.push(format!("{fid} KEEP\n{fid} REFUTED")),
            _ => {}
        }
        if rng.chance(35) {
            let words: Vec<&str> = body.split(' ').collect();
            let sentence = if rng.chance(70) && words.len() > 1 {
                let i = rng.below(words.len());
                let j = (i + 1 + rng.below(3)).min(words.len());
                words[i..j].join(" ")
            } else {
                rng.pick(SENTENCES).to_string()
            };
            if !sentence.contains('\n') && !sentence.is_empty() {
                lines.push(format!("{fid} CUT \"{sentence}\""));
            }
        }
    }
    if rng.chance(20) {
        lines.push(fill(rng.pick(VERDICT_LINES), rng));
    }
    let sep = if rng.chance(10) { "\r\n" } else { "\n" };
    lines.join(sep)
}

fn plan_with(passes: Vec<Value>, kind: &str) -> Value {
    json!({"version": 1, "kind": kind, "role": "ROLE", "passes": passes})
}

fn pass(id: &str, reply: &str, inputs: &[&str]) -> Value {
    json!({"id": id, "prompt": format!("{id}.md"), "reply": reply, "inputs": inputs, "text": format!("INSTRUCTION {id}")})
}

fn checked_plans() -> Vec<Value> {
    let mut k = pass("k", "verdicts", &["a", "b"]);
    let mut batched = k.clone();
    batched["batch"] = json!(2);
    k["model"] = json!("small");
    vec![
        plan_with(vec![pass("a", "findings", &[]), pass("b", "findings", &[]), batched], "review"),
        plan_with(vec![pass("a", "findings", &[]), pass("b", "findings", &[]), k], "review"),
        plan_with(vec![pass("r", "findings", &[]), pass("k", "verdicts", &[])], "review"),
        plan_with(vec![pass("r", "findings", &[])], "review"),
    ]
}

#[test]
fn reply_texts_match_python() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let plans = checked_plans();
    let mut cases = vec![case("consts", vec![])];
    let mut texts: Vec<String> = (0..1200).map(|_| text(&mut rng)).collect();
    texts.extend((0..300).map(|_| {
        let n = rng.below(6);
        review(&mut rng, n)
    }));
    let many: String = (1..=pl::MAX_FINDINGS + 1).map(|i| format!("### {i}. Bug {i} (`a.py:{i}`)\n- Problem: p.\n\n")).collect();
    let exactly: String = (1..=pl::MAX_FINDINGS).map(|i| format!("## {i}) Bug {i}\n")).collect();
    texts.extend([many, exactly, String::new(), " \n\t".into(), "\r\n".into(), "No defects found.".into()]);
    texts.extend(fixtures());
    for t in &texts {
        let t = Value::String(t.clone());
        for op in ["unfenced", "has_issues", "verdicts", "finding_id", "no_defects", "head_match"] {
            cases.push(case(op, vec![t.clone()]));
        }
        cases.push(case("findings", vec![json!("a"), t.clone()]));
        cases.push(case("scope_verdicts", vec![t.clone(), json!(["a.1", "b.2", "k-x.3", "\u{e9}.1"])]));
        for rx in ["HEAD", "SECTION", "VERDICT", "CUT"] {
            cases.push(case("finditer", vec![json!(rx), t.clone()]));
        }
        for plan in &plans {
            for i in 0..plan["passes"].as_array().unwrap().len() {
                cases.push(case("check_reply", vec![plan.clone(), json!(i), t.clone()]));
            }
        }
    }
    // The masked text is what the headings are searched in: run the regexes on it too.
    for t in texts.iter().take(400) {
        let masked = pl::unfenced(t).1;
        for rx in ["HEAD", "SECTION"] {
            cases.push(case("finditer", vec![json!(rx), json!(masked)]));
        }
    }
    gate("reply texts", &cases);
}

/// Hand-written replies in the shapes the Python tests use.
fn fixtures() -> Vec<String> {
    let review = "What this change does: x.\n\n### 1. Bug one (`a.py:1`)\n- Code: `a`\n- Problem: p1. Side claim here.\n\n### 2. Bug two (`a.py:2`)\n- Code: `b`\n- Problem: p2.\n\n### Not checked\n- runtime";
    let mut out = vec![
        review.to_string(),
        "### 1. Defect\np\n### Open questions\n### 1. Is the service reachable?\nUnknown.\n### Not checked\nruntime".into(),
        "### Open questions\n- none\n### 1. Real bug\nEvidence.".into(),
        "### Not checked\n- none\n### 1. Real bug\nEvidence.".into(),
        "Summary.\nNo defects found.\n### Open questions\n- Is it deployed?\n### Not checked\n- runtime".into(),
        "a.1: **KEEP** - yes\nb.1 `DUP` of **a.1**\na.1 **CUT** \"e\"".into(),
        "NONE\n- Actually a defect".into(),
        "What this change does: x.\n\n### 1. Cache is read without the lock\n- Location: `src/pool.py:4-6`\n```\n### 6. Not a finding inside a fence (`fenced.py:1`)\n```\n\n### 2. Workflow and docs disagree\n\n### Open questions\n### 7. Numbered question (`q.py:1`)\n\n### Not checked\n- `n.py:3`\n".into(),
    ];
    for heading in ["#### Not checked for null", "#### Open questions about ownership", "#### Not checked"] {
        out.push(format!("### 1. Null dereference\nEvidence.\n{heading}\nExplanation.\n### 2. Leak\nMore evidence."));
    }
    for fence in ["```markdown", "~~~~"] {
        out.push(format!("### 1. Real bug\nExample:\n{fence}\n### 2. Sample data\n### Not checked\nQuoted content"));
    }
    for fence in ["```markdown", "~~~~markdown"] {
        let closing = if fence.starts_with('`') { &fence[..3] } else { "~~~~~" };
        let body = format!("### 1. Defect\n- Code:\n{fence}\n## 2. Run setup\n## Not checked\n{closing}\n- Problem: broken\n- Fix: restore");
        out.push(format!("`````markdown\n{body}\n`````"));
        out.push(body);
    }
    for t in ["NONE - no issues", "**NONE**", "`NONE`.", "None.\n", "none: fine", "NONE\u{2013} x", "NONE- x", "NONE -x"] {
        out.push(t.into());
    }
    out
}

#[test]
fn unicode_classes_match_python() {
    // Every code point Python's re puts in \w, \s, \d against the classes the port spells out; and each
    // case-insensitive word with each letter swapped for every character whose case mapping touches ASCII.
    let all: String = (0..0x110000u32).filter_map(char::from_u32).filter(|c| *c != '\n').collect();
    let script = r#"
import re, sys, json
s = ''.join(chr(c) for c in range(0x110000) if not 0xd800 <= c <= 0xdfff and c != 10)
def runs(p):
    return [[m.start(), m.end()] for m in re.finditer(p, s)]
cands = sorted({c for c in s if any(x.isascii() and x.isalpha() for x in c.lower() + c.upper() + c.casefold())})
print(json.dumps([runs(r'\w+'), runs(r'\s+'), runs(r'\d+'), cands]))
"#;
    let out = Command::new("python3").arg("-I").arg("-c").arg(script).output().unwrap();
    assert!(out.status.success());
    let want = pyjson::loads(&String::from_utf8(out.stdout).unwrap()).unwrap();
    let runs = |p: &str| -> Value {
        let rx = regex::Regex::new(p).unwrap();
        let mut at = 0usize;
        let mut chars = 0usize;
        let mut out = Vec::new();
        for m in rx.find_iter(&all) {
            chars += all[at..m.start()].chars().count();
            let len = m.as_str().chars().count();
            out.push(json!([chars, chars + len]));
            chars += len;
            at = m.end();
        }
        Value::Array(out)
    };
    assert_eq!(runs(&format!("[{}]+", pl::WORD)), want[0], "\\w");
    assert_eq!(runs(&format!("[{}]+", pl::SPACE)), want[1], "\\s");
    assert_eq!(runs(r"\d+"), want[2], "\\d");
    let cands: Vec<char> = want[3].as_array().unwrap().iter().map(|c| st(c).chars().next().unwrap()).collect();
    eprintln!("case candidates: {}", cands.len());
    let mut cases = Vec::new();
    for word in ["Open questions", "Not checked", "No defects found", "NONE"] {
        for (i, _) in word.char_indices() {
            for c in &cands {
                let mut w = word.to_string();
                w.replace_range(i..i + 1, &c.to_string());
                let heading = format!("### {w}");
                cases.push(case("finditer", vec![json!("SECTION"), json!(heading)]));
                cases.push(case("no_defects", vec![json!(w)]));
                cases.push(case("has_issues", vec![json!(w)]));
            }
        }
    }
    gate("case-insensitive words", &cases);
}

fn findings_items(plan: &Value, replies: &Replies) -> Vec<(String, String)> {
    pl::all_findings(plan, replies).into_iter().collect()
}

#[test]
fn assembly_matches_python() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let plans = checked_plans();
    let design = plan_with(
        vec![
            pass("draft", "proposal", &[]),
            pass("nums", "text", &[]),
            pass("audit", "issues", &["draft", "nums"]),
            json!({"id": "revise", "prompt": "r.md", "reply": "proposal", "inputs": ["draft", "audit"], "when": "audit", "text": "INSTRUCTION revise"}),
        ],
        "design",
    );
    let mut cases = Vec::new();
    for _ in 0..700 {
        let plan = rng.pick(&plans).clone();
        let mut r = Replies::new();
        for p in plan["passes"].as_array().unwrap() {
            if p["reply"] == "findings" && rng.chance(92) {
                let n = rng.below(5);
                r.insert(st(&p["id"]).into(), review(&mut rng, n));
            }
        }
        let items = findings_items(&plan, &r);
        let checker_id = plan["passes"].as_array().unwrap().iter().find(|p| p["reply"] == "verdicts").map(|p| st(&p["id"]).to_string());
        if let Some(k) = checker_id
            && rng.chance(95)
        {
            r.insert(k, checker(&mut rng, &items));
        }
        let rv = json!(r);
        cases.push(case("assemble", vec![plan.clone(), rv.clone()]));
        cases.push(case("all_findings", vec![plan.clone(), rv.clone()]));
        for (i, _) in plan["passes"].as_array().unwrap().iter().enumerate() {
            cases.push(case("instruction", vec![plan.clone(), json!(i), rv.clone()]));
            cases.push(case("requests", vec![plan.clone(), json!(i), rv.clone()]));
        }
    }
    // The Python tests' scenarios, written out.
    let review_a = fixtures()[0].clone();
    let review_b = review_a.replace("Bug one", "Bug three").replace("Bug two", "Bug four");
    let p = plans[0].clone();
    for k in [
        "a.1 KEEP - q\na.1 CUT \"Side claim here.\"\na.2 REFUTED - q\nb.1 DUP a.1 - same\nb.2 OPEN - needs unseen code",
        "a.1 KEEP\na.2 KEEP\nb.1 KEEP\nb.2 KEEP",
        "a.1 KEEP\na.2 DUP a.1\nb.1 DUP a.2\nb.2 KEEP",
        "a.1 KEEP\na.2 KEEP\nb.1 DUP ghost.1\nb.2 KEEP\nghost.1 KEEP",
        "a.1 KEEP\na.2 KEEP\nb.1 KEEP",
        "a.1 REFUTED\na.2 KEEP\nb.1 DUP a.1\nb.2 KEEP",
        "a.1 REFUTED\na.2 KEEP\nb.1 DUP a.1",
        "a.1 KEEP\na.2 REFUTED\na.1 CUT \"e\"",
    ] {
        cases.push(case("assemble", vec![p.clone(), json!({"a": review_a, "b": review_b, "k": k})]));
        cases.push(case("assemble", vec![p.clone(), json!({"a": "No defects found.", "b": review_b, "k": k})]));
    }
    let repeated = review_a.replace("Side claim here.", "Side claim here.\n```python\n# Side claim here.\n```");
    cases.push(case("assemble", vec![p.clone(), json!({"a": repeated, "b": "No defects found.", "k": "a.1 KEEP\na.2 REFUTED\na.1 CUT \"Side claim here.\""})]));
    cases.push(case("assemble", vec![p.clone(), json!({})]));
    cases.push(case("assemble", vec![json!({"kind": "design", "passes": [{"id": "draft", "reply": "proposal"}]}), json!({"audit": "NONE"})]));
    for audit in ["NONE", "the draft misses the 3x growth"] {
        let mut r = json!({"draft": " DRAFT \n", "nums": "NUMBERS", "audit": audit});
        cases.push(case("assemble", vec![design.clone(), r.clone()]));
        r["revise"] = json!("REVISED");
        cases.push(case("assemble", vec![design.clone(), r.clone()]));
        for i in 0..4 {
            cases.push(case("instruction", vec![design.clone(), json!(i), r.clone()]));
            cases.push(case("requests", vec![design.clone(), json!(i), r.clone()]));
        }
    }
    let lines = gate("assembly", &cases);
    let need = lines.iter().filter(|l| l.contains("\"NeedVerdicts\"")).count();
    let kept = lines.iter().filter(|l| l.contains("\"kept\": ")).count();
    eprintln!("assembly: {need} NeedVerdicts, {kept} assembled");
    assert!(need > 20 && kept > 100);
}

#[test]
fn batches_and_repairs_match_python() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let mut cases = Vec::new();
    let big: Value = pyjson::loads("1000000000000000000000000000000").unwrap();
    for _ in 0..400 {
        let inputs: Vec<&str> = ["a", "b", "facts"].into_iter().filter(|_| rng.chance(60)).collect();
        let mut k = pass("k", "verdicts", &inputs);
        let batch = [Value::Null, json!(1), json!(2), json!(3), json!(100), big.clone()][rng.below(6)].clone();
        if !batch.is_null() {
            k["batch"] = batch;
        }
        let mut facts = pass("facts", "text", &[]);
        let bare = rng.chance(30); // as in the Python tests: an unvalidated pass, read only through others
        if bare {
            facts.as_object_mut().unwrap().remove("text");
            facts.as_object_mut().unwrap().remove("inputs");
        }
        let plan = plan_with(vec![pass("a", "findings", &[]), pass("b", "findings", &["a"]), facts, k], "review");
        let mut r = Replies::new();
        for id in ["a", "b"] {
            if rng.chance(85) {
                let n = rng.below(7);
                r.insert(id.into(), review(&mut rng, n));
            }
        }
        if rng.chance(70) {
            r.insert("facts".into(), "INDEPENDENT EVIDENCE".into());
        }
        let items = findings_items(&plan, &r);
        let rv = json!(r);
        let n = pl::requests(&plan, &plan["passes"][3], &r).len() + rng.below(2);
        let texts: Vec<String> = (0..n).map(|_| checker(&mut rng, &items)).collect();
        cases.push(case("combine", vec![plan.clone(), json!(3), rv.clone(), json!(texts)]));
        cases.push(case("combine", vec![plan.clone(), json!(0), rv.clone(), json!(["ONE", "TWO"])]));
        for i in (0..4).filter(|&i| !(bare && i == 2)) {
            cases.push(case("requests", vec![plan.clone(), json!(i), rv.clone()]));
        }
        let mut need: Vec<String> = items.iter().filter(|_| rng.chance(40)).map(|(f, _)| f.clone()).collect();
        if rng.chance(20) {
            need.push("ghost.1".into());
        }
        let reason = rng.pick(&["no verdict", "duplicate of a finding that was not kept", "no verdict and duplicate of a finding that was not kept", "missing"]);
        cases.push(case("repair_request", vec![plan.clone(), json!(3), rv.clone(), json!(need), json!(reason)]));
        cases.push(case("need", vec![json!(need), json!(reason)]));
        let extra = checker(&mut rng, &items);
        cases.push(case("scope_verdicts", vec![json!(extra), json!(need)]));
    }
    gate("batches and repairs", &cases);
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// A folder of prompt files covering each way a prompt can be found, missing, unreadable or outside.
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("ringo-gate-pipelines-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let prompts = dir.join("prompts");
        fs::create_dir_all(prompts.join("dir.md")).unwrap();
        for n in ["a", "b", "k", "i", "d", "r", "n", "audit", "draft", "\u{e9}", ".", "a.md", "x_y-z"] {
            fs::write(prompts.join(format!("{n}.md")), format!("INSTRUCTION {n}\n")).unwrap();
        }
        let files: &[(&str, &[u8])] = &[
            ("crlf.md", b"line1\r\nline2\rline3\r\n"),
            ("bom.md", "\u{feff}BOM text".as_bytes()),
            ("bad.md", b"ok\xe2\x82"),
            ("bad2.md", b"\xff"),
            ("bad3.md", b"a\xf0\x9f\x98A"),
            ("ws.md", b"  \n\t spaced \x1c\n"),
            ("empty.md", b""),
            ("noperm.md", b"secret"),
        ];
        for (n, b) in files {
            fs::write(prompts.join(n), b).unwrap();
        }
        fs::set_permissions(prompts.join("noperm.md"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::write(dir.join("outside.md"), "OUTSIDE").unwrap();
        symlink("../outside.md", prompts.join("link.md")).unwrap();
        symlink("a.md", prompts.join("inner.md")).unwrap();
        symlink("nowhere.md", prompts.join("broken.md")).unwrap();
        symlink("loop.md", prompts.join("loop.md")).unwrap();
        symlink("prompts", dir.join("linked")).unwrap();
        symlink("../prompts/../prompts", prompts.join("up")).unwrap();
        Fixture { dir }
    }

    fn path(&self, rel: &str) -> String {
        self.dir.join(rel).to_string_lossy().into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.dir.join("prompts/noperm.md"), fs::Permissions::from_mode(0o600));
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn number(text: &str) -> Value {
    pyjson::loads(text).unwrap()
}

const PROMPTS: &[&str] = &[
    "a.md", "b.md", "k.md", "i.md", "../x.md", "missing.md", "a.txt", "sub/a.md", ".md", "..md", "a.md.md", "\u{e9}.md",
    "link.md", "inner.md", "dir.md", "broken.md", "crlf.md", "bom.md", "bad.md", "bad2.md", "bad3.md", "ws.md", "empty.md",
    "noperm.md", "loop.md", "x_y-z.md", "a b.md", "A.MD",
];

fn random_pass(rng: &mut Rng, earlier: &[String]) -> Value {
    if rng.chance(3) {
        return [json!("x"), json!(5), Value::Null, json!([]), json!({"id": "a", "zz": [1, 2.5, true, null, "it's"]})][rng.below(5)].clone();
    }
    let mut m = Map::new();
    let earlier_id = earlier.get(rng.below(earlier.len().max(1))).cloned().unwrap_or_else(|| "a".into());
    if rng.chance(96) {
        let ids = [json!("a"), json!("b"), json!("k"), json!("audit"), json!("draft"), json!("a b"), json!("1a"), json!(""),
                   json!(5), Value::Null, json!("\u{e9}"), json!("a-b"), json!("a_1"), json!("x\u{2000}"), json!(earlier_id.clone()),
                   json!("r"), json!("n"), json!("i"), json!("d"), json!(["a"])];
        let id = if rng.chance(50) { json!(format!("p{}", rng.below(1000))) } else { rng.pick(&ids).clone() };
        m.insert("id".into(), id);
    }
    if rng.chance(95) {
        m.insert("prompt".into(), if rng.chance(70) { json!(rng.pick(&PROMPTS[..4])) } else if rng.chance(90) { json!(rng.pick(PROMPTS)) } else { json!(5) });
    }
    if rng.chance(96) {
        let replies = [json!("findings"), json!("verdicts"), json!("issues"), json!("proposal"), json!("text"), json!("x"), Value::Null, json!("Findings")];
        m.insert("reply".into(), if rng.chance(80) { json!(rng.pick(&pl::REPLIES[..4])) } else { rng.pick(&replies).clone() });
    }
    if rng.chance(40) {
        let opts = [json!([]), json!([earlier_id.clone()]), json!(["z"]), json!([[]]), json!("a"), json!([5]), json!(earlier.iter().take(2).collect::<Vec<_>>())];
        m.insert("inputs".into(), rng.pick(&opts).clone());
    }
    if rng.chance(20) {
        let opts = [json!(earlier_id.clone()), json!("audit"), json!([]), Value::Null, json!("zz")];
        m.insert("when".into(), rng.pick(&opts).clone());
    }
    if rng.chance(20) {
        let opts = [json!(1), json!(2), json!(0), json!(-1), json!(true), number("1.0"), number("1e2"), json!("2"),
                    number("1000000000000000000000000000000"), json!(3), number("-0")];
        m.insert("batch".into(), rng.pick(&opts).clone());
    }
    if rng.chance(15) {
        let opts = [json!("small"), json!("claude-3.5:x"), json!("a b"), json!(""), json!(5), json!("\u{e9}"), json!("m\n")];
        m.insert("model".into(), rng.pick(&opts).clone());
    }
    if rng.chance(3) {
        m.insert(rng.pick(&["extra", "text", "Id"]).to_string(), json!(1));
    }
    if rng.chance(30) {
        let keys: Vec<String> = m.keys().cloned().collect();
        let mut shuffled = Map::new();
        for k in keys.iter().rev() {
            shuffled.insert(k.clone(), m[k].clone());
        }
        m = shuffled;
    }
    Value::Object(m)
}

fn random_plan(rng: &mut Rng) -> Value {
    if rng.chance(2) {
        let opts = [json!([]), json!(["version"]), json!(["b", "a", "b"]), json!("abc"), json!(""), json!(5), number("1.5"), json!(true),
                    Value::Null, json!([{}]), json!([[1]]), json!(["kind", "zeta"])];
        return rng.pick(&opts).clone();
    }
    let mut m = Map::new();
    if rng.chance(96) {
        let opts = [json!(1), number("1.0"), json!(true), json!(2), json!("1"), Value::Null, number("1e0"), number("10e-1"), json!(0)];
        m.insert("version".into(), if rng.chance(85) { json!(1) } else { rng.pick(&opts).clone() });
    }
    if rng.chance(96) {
        let opts = [json!("review"), json!("design"), json!("x"), Value::Null, json!(["review"])];
        m.insert("kind".into(), if rng.chance(90) { json!(rng.pick(&["review", "design"])) } else { rng.pick(&opts).clone() });
    }
    if rng.chance(96) {
        let opts = [json!("You review code."), json!(""), json!("  "), json!("\u{1c}\u{a0}"), json!(5), json!("r")];
        m.insert("role".into(), if rng.chance(85) { json!("ROLE") } else { rng.pick(&opts).clone() });
    }
    if rng.chance(97) {
        if rng.chance(4) {
            m.insert("passes".into(), [json!([]), Value::Null, json!("x"), json!({})][rng.below(4)].clone());
        } else {
            let mut earlier: Vec<String> = Vec::new();
            let mut ps = Vec::new();
            for _ in 0..1 + rng.below(5) {
                let p = random_pass(rng, &earlier);
                if let Some(id) = p.get("id").and_then(Value::as_str) {
                    earlier.push(id.to_string());
                }
                ps.push(p);
            }
            m.insert("passes".into(), Value::Array(ps));
        }
    }
    if rng.chance(8) {
        m.insert("prompts_dir".into(), json!("x"));
    }
    if rng.chance(4) {
        m.insert(rng.pick(&["extra", "zeta", "\u{c9}clair", "Version"]).to_string(), json!(1));
    }
    Value::Object(m)
}

/// Plans that validate on the Python side: a findings reader or two and maybe a checker, or a design chain.
fn valid_plan(rng: &mut Rng) -> Value {
    let mut ps = vec![json!({"id": "a", "prompt": "a.md", "reply": "findings"})];
    if rng.chance(50) {
        ps.push(json!({"id": "audit", "prompt": "i.md", "reply": "issues", "inputs": ["a"]}));
    }
    if rng.chance(60) {
        ps.push(json!({"id": "b", "prompt": "b.md", "reply": "findings", "model": "small"}));
    }
    if ps.len() > 1 || rng.chance(50) {
        let mut k = json!({"id": "k", "prompt": "k.md", "reply": "verdicts", "inputs": ["a"]});
        if rng.chance(50) {
            k["batch"] = json!(1 + rng.below(3));
        }
        ps.push(k);
    }
    if ps.iter().any(|p| p["id"] == "audit") && rng.chance(60) {
        ps.push(json!({"id": "fix", "prompt": "r.md", "reply": "text", "inputs": ["audit"], "when": "audit"}));
    }
    json!({"version": 1, "kind": "review", "role": "You review code changes.", "passes": ps})
}

#[test]
fn validation_matches_python() {
    let fx = Fixture::new("validate");
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let dirs = [fx.path("prompts"), fx.path("linked"), fx.path("missing"), fx.path(""), fx.path("prompts/up"), fx.path("prompts/")];
    let mut cases = Vec::new();
    for i in 0..2500 {
        let plan = if i % 5 == 0 { valid_plan(&mut rng) } else { random_plan(&mut rng) };
        let dir = if rng.chance(85) { dirs[0].clone() } else { rng.pick(&dirs).clone() };
        cases.push(case("validate", vec![plan, json!(dir)]));
    }
    // Design plans, including one whose passes cannot be ordered.
    let design = |passes: Value| json!({"version": 1, "kind": "design", "role": "R", "passes": passes});
    for passes in [
        json!([{"id": "draft", "prompt": "d.md", "reply": "proposal"}, {"id": "nums", "prompt": "n.md", "reply": "text"},
               {"id": "audit", "prompt": "a.md", "reply": "issues", "inputs": ["draft", "nums"]},
               {"id": "revise", "prompt": "r.md", "reply": "proposal", "inputs": ["draft", "audit"], "when": "audit"}]),
        json!([{"id": "d", "prompt": "d.md", "reply": "proposal"}, {"id": "k", "prompt": "k.md", "reply": "verdicts"},
               {"id": "f", "prompt": "a.md", "reply": "findings", "inputs": ["k"]}]),
        json!([{"id": "d", "prompt": "d.md", "reply": "proposal"}, {"id": "k", "prompt": "k.md", "reply": "verdicts"},
               {"id": "f", "prompt": "a.md", "reply": "findings"}, {"id": "g", "prompt": "b.md", "reply": "findings", "inputs": ["f"]}]),
        json!([{"id": "audit", "prompt": "a.md", "reply": "issues"}, {"id": "out", "prompt": "b.md", "reply": "proposal", "when": "audit"}]),
    ] {
        cases.push(case("validate", vec![design(passes), json!(dirs[0])]));
    }
    for plan in explicit_plans() {
        cases.push(case("validate", vec![plan, json!(dirs[0])]));
    }
    let lines = gate("validate", &cases);
    // Every message validate() raises, and the OS and decoding errors of reading a prompt, came up.
    for needle in [
        "a plan needs version 1 and kind review or design; unknown keys: []", "unknown keys: ['", "a plan needs a role line",
        "a plan needs at least one pass", ": unknown keys\"", "is missing, malformed or repeated", "reply must be one of",
        "inputs must name earlier passes", "when must name an earlier issues pass", "batch is a positive integer",
        "model is a model name", "prompt must be a .md file name", "not found in", "a review plan needs one or more findings",
        "the verdicts pass must come after every findings pass", "a review plan needs a findings pass without when",
        "a design plan needs a proposal pass without when", "Permission denied", "codec can't decode", "not iterable",
        "has no attribute 'get'", "unhashable type",
    ] {
        let n = lines.iter().filter(|l| l.contains(needle)).count();
        eprintln!("  {n:5}  {needle}");
        assert!(n > 0, "no case raised {needle:?}");
    }
    let valid: Vec<Value> = lines.iter().filter(|l| l.starts_with("{\"ok\"")).map(|l| pyjson::loads(l).unwrap()["ok"].clone()).collect();
    eprintln!("validate: {} plans valid", valid.len());
    assert!(valid.len() > 300);
    let waves: Vec<(String, Vec<Value>)> = valid.into_iter().map(|p| case("waves", vec![p])).collect();
    let lines = gate("waves", &waves);
    assert!(lines.iter().any(|l| l.contains("passes cannot be ordered")));
}

/// One plan per validate() message, in the shapes the Python tests use.
fn explicit_plans() -> Vec<Value> {
    let f = |id: &str| json!({"id": id, "prompt": format!("{id}.md"), "reply": "findings"});
    let review = |passes: Value| json!({"version": 1, "kind": "review", "role": "r", "passes": passes});
    let mut out = vec![
        json!({"version": 2, "kind": "review", "role": "r", "passes": [f("a")]}),
        json!({"version": 1, "kind": "work", "role": "r", "passes": [f("a")]}),
        json!({"version": 1, "kind": "review", "role": "r", "passes": [f("a")], "extra": 1, "Zeta": 2}),
        json!({"version": true, "kind": "review", "role": "r", "passes": [f("a")]}),
        json!({"version": 1, "kind": "review", "role": " \u{1c} ", "passes": [f("a")]}),
        json!({"version": 1, "kind": "review", "passes": [f("a")]}),
        json!({"version": 1, "kind": "review", "role": "r", "passes": []}),
        json!({"version": 1, "kind": "review", "role": "r"}),
        review(json!([f("a"), f("a")])),
        review(json!([f("a"), f("b")])),
        review(json!([{"id": "a", "prompt": "a.md", "reply": "findings", "inputs": ["z"]}])),
        review(json!([{"id": "a", "prompt": "a.md", "reply": "findings", "inputs": [[]]}])),
        review(json!([{"id": "a", "prompt": "a.md", "reply": "findings", "when": []}])),
        review(json!([{"id": "a", "prompt": "../x.md", "reply": "findings"}])),
        review(json!([{"id": "a", "prompt": "a.md", "reply": "findings", "extra": 1}])),
        review(json!([{"id": "k", "prompt": "k.md", "reply": "verdicts"}, f("a"), f("b")])),
        review(json!([{"id": "a", "prompt": "a.md", "reply": "findings", "batch": 2}])),
        review(json!([f("a"), {"id": "k", "prompt": "k.md", "reply": "verdicts", "batch": 0}])),
        review(json!([f("a"), {"id": "k", "prompt": "k.md", "reply": "verdicts", "batch": true}])),
        review(json!([f("a"), {"id": "k", "prompt": "k.md", "reply": "verdicts", "model": "a b"}])),
        review(json!([f("a"), {"id": "k", "prompt": "k.md", "reply": "nope"}])),
        review(json!([{"id": "audit", "prompt": "a.md", "reply": "issues"}, {"id": "out", "prompt": "b.md", "reply": "findings", "when": "audit"}])),
        review(json!([f("a"), {"id": "audit", "prompt": "i.md", "reply": "issues"}, {"id": "k", "prompt": "k.md", "reply": "verdicts", "when": "audit"}])),
        review(json!([f("a"), {"id": "k", "prompt": "k.md", "reply": "verdicts"}, {"id": "k2", "prompt": "k.md", "reply": "verdicts"}])),
        review(json!([{"id": "a", "prompt": "noperm.md", "reply": "findings"}])),
        review(json!([{"id": "a", "prompt": "bad3.md", "reply": "findings"}])),
        review(json!([{"id": "a", "prompt": "link.md", "reply": "findings"}])),
        review(json!([{"id": "a", "prompt": "inner.md", "reply": "findings"}, {"id": "b", "prompt": "crlf.md", "reply": "findings"},
                      {"id": "c", "prompt": "bom.md", "reply": "findings"}, {"id": "k", "prompt": "ws.md", "reply": "verdicts", "batch": 2}])),
        review(json!([{"id": "a", "prompt": "loop.md", "reply": "findings"}])),
        review(json!([{"id": "a", "prompt": "dir.md", "reply": "findings"}])),
        review(json!([{"id": "a", "prompt": "broken.md", "reply": "findings"}])),
        review(json!(["x"])),
        review(json!([{"id": "a\u{2000}b'\"", "prompt": "a.md", "reply": "findings"}])),
        json!({"version": 1, "kind": "design", "role": "r", "passes": [{"id": "audit", "prompt": "a.md", "reply": "issues"},
               {"id": "out", "prompt": "b.md", "reply": "proposal", "when": "audit"}]}),
    ];
    for v in [json!([]), json!(["version"]), json!(["b", "a", "b"]), json!("abc"), json!(""), json!(5), json!(true), Value::Null,
              json!([{}]), json!([[1]]), json!(["kind", "zeta"])] {
        out.push(v);
    }
    out
}

/// Whether Python's json.loads accepts each document.
fn python_parses(docs: &[String]) -> Vec<bool> {
    let script = "import json, sys\nfor d in json.load(sys.stdin):\n    try:\n        json.loads(d); print(1)\n    except ValueError:\n        print(0)\n";
    let mut child = Command::new("python3").args(["-I", "-c", script]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let text = pyjson::dumps(&json!(docs));
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(text.as_bytes()));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    String::from_utf8(out.stdout).unwrap().lines().map(|l| l == "1").collect()
}

/// Mutations of a JSON document, for json.loads error texts.
fn mutate(rng: &mut Rng, doc: &str) -> String {
    const TOKENS: &[&str] = &[
        ",", ":", "{", "}", "[", "]", "\"", "\\", "\\u12", "\\uzzzz", "\\x", "\u{1}", "-", "0", "01", "1.", "1e", "1e+", "-0.5e-3",
        "tru", "null", " ", "\n", "\t", "\r", "\u{e9}", "\\ud800\\udc00", "\\n", "\"k\": 1", "]]", "{}", "NaN", "-Inf",
    ];
    let mut chars: Vec<char> = doc.chars().collect();
    for _ in 0..1 + rng.below(3) {
        let i = rng.below(chars.len() + 1);
        match rng.below(3) {
            0 if i < chars.len() => {
                chars.remove(i);
            }
            1 if i < chars.len() => {
                chars.splice(i..i + 1, rng.pick(TOKENS).chars());
            }
            _ => {
                chars.splice(i..i, rng.pick(TOKENS).chars());
            }
        }
    }
    let mut out: String = chars.into_iter().collect();
    if rng.chance(2) {
        out.insert(0, '\u{feff}');
    }
    out
}

#[test]
fn plan_files_match_python() {
    let fx = Fixture::new("load");
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut cases = vec![
        case("load_plan", vec![Value::Null, json!("review")]),
        case("load_plan", vec![Value::Null, json!("design")]),
        case("load_plan", vec![Value::Null, json!("work")]),
        case("load_plan", vec![json!(""), json!("review")]),
    ];
    for name in ["review.json", "design.json", "review-doc.json"] {
        let path = root().join("plans").join(name).to_string_lossy().into_owned();
        for kind in ["review", "design"] {
            cases.push(case("load_plan", vec![json!(path), json!(kind)]));
        }
    }
    let rel_missing = "a//b/./missing.json";
    for p in [fx.path("missing.json"), rel_missing.into(), fx.path("prompts"), "./nope.json".into(), "//nope/x.json".into()] {
        cases.push(case("load_plan", vec![json!(p), json!("review")]));
    }
    let base = json!({"version": 1, "kind": "review", "role": "You review code changes.",
                      "passes": [{"id": "review", "prompt": "a.md", "reply": "findings"}]});
    let mut docs: Vec<String> = Vec::new();
    for dir in ["prompts", "", ".", "linked", "missing", "../", "/nonexistent", "prompts/up", "x\u{0}y"] {
        let mut plan = base.clone();
        plan["prompts_dir"] = json!(dir);
        docs.push(pyjson::dumps(&plan));
    }
    let shapes = [pyjson::dumps(&base), pyjson::dumps_indent(&base, 1), pyjson::dumps_compact(&base), "[1, 2]".into(), "\"x\"".into(),
                  "{\"kind\": \"design\"}".into(), "{\"kind\": \"review\", \"version\": 1}".into()];
    docs.extend(shapes.iter().cloned());
    for _ in 0..1500 {
        let doc = rng.pick(&shapes).clone();
        docs.push(mutate(&mut rng, &doc));
    }
    docs.push(format!("[{}]", "1".repeat(4301)));
    docs.push(format!("[{}, x]", "-".to_string() + &"9".repeat(4301)));
    docs.push(format!("[{}e1]", "1".repeat(4301)));
    docs.push("{\"a\": 1}\r\n".into());
    // String escapes at each boundary the C scanner checks (\\u spelled with a variable).
    let u = "\\".to_string() + "u";
    for d in [
        format!("\"{u}1234"), format!("\"{u}123"), format!("\"{u}12\""), format!("\"{u}123\""), format!("[\"{u}d800{u}12\"]"),
        format!("[\"{u}d800{u}dc0\"]"), format!("[\"{u}d800{u}dc00"), format!("[\"{u}d800{u}"), format!("[\"{u}d800\\"),
        format!("[\"{u}d800{u}zzzz\"]"), format!("[\"{u}d800{u}0041\"]"), format!("[\"{u}D83D{u}DE00\", 1,]"), format!("\"{u}\""),
        "\"\\".into(), "\"\\q\"".into(), "\"a\u{1}\"".into(), "\"a\tb\"".into(), "[1 2]".into(), "{\"a\" 1}".into(), "{1: 2}".into(),
        "{\"a\": 1,}".into(), "[1,]".into(), "[1,\n2,,]".into(), "{\"a\":".into(), "[".into(), "{".into(), "".into(), "  ".into(),
        "01".into(), "1.".into(), "1e".into(), "1e+".into(), "-".into(), "-a".into(), "nul".into(), "{}x".into(), "\"abc".into(),
        "[-0.0e-0, 1E9, 0.5]".into(), "[tru]".into(), "[nulll]".into(), "\u{feff}{}".into(), "{\"a\": [1, {\"b\": }]}".into(),
    ] {
        docs.push(d);
    }
    // Documents Python parses but serde cannot hold (NaN, Infinity, lone surrogates) are left out.
    let parses = python_parses(&docs);
    let mut skipped = 0;
    for (i, doc) in docs.iter().enumerate() {
        if parses[i] && pyjson::loads(doc).is_err() {
            skipped += 1;
            continue;
        }
        let file = fx.dir.join(format!("plan-{i}.json"));
        fs::write(&file, doc).unwrap();
        cases.push(case("load_plan", vec![json!(file.to_string_lossy()), json!("review")]));
    }
    for (name, bytes) in [("bad.json", &b"{\"kind\": \"review\"\xff}"[..]), ("crlf.json", b"{\"kind\":\r\n \"x\"}"), ("trunc.json", b"{\"k\xe2")] {
        let file = fx.dir.join(name);
        fs::write(&file, bytes).unwrap();
        cases.push(case("load_plan", vec![json!(file.to_string_lossy()), json!("review")]));
    }
    let lines = gate("load_plan", &cases);
    let errors = lines.iter().filter(|l| l.contains("(char ")).count();
    eprintln!("load_plan: {errors} JSON errors compared, {skipped} documents Python parses and serde cannot left out");
    assert!(errors > 800);
}

/// Text corpora for the sweeps: repository files, every commit's message and patch, and the Python standard library.
fn corpus() -> Vec<String> {
    let mut out = Vec::new();
    let stdlib = Command::new("python3").args(["-I", "-c", "import sysconfig; print(sysconfig.get_paths()['stdlib'])"]).output().unwrap();
    let stdlib = PathBuf::from(String::from_utf8(stdlib.stdout).unwrap().trim());
    let mut stack = vec![root(), stdlib];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "target" || name == "__pycache__" || name == "site-packages" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = fs::read_to_string(&path)
                && text.len() < 400_000
            {
                out.push(text);
            }
        }
    }
    let log = Command::new("git").arg("-C").arg(root()).args(["log", "--all", "-p", "--format=%x00%B"]).output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout).into_owned();
    out.extend(log.split('\0').filter(|s| !s.trim().is_empty() && s.len() < 400_000).map(str::to_string));
    out
}

#[test]
#[ignore]
fn corpus_sweep_matches_python() {
    let texts = corpus();
    let plans = checked_plans();
    let mut cases = Vec::new();
    for t in &texts {
        let t = json!(t);
        for op in ["unfenced", "has_issues", "verdicts", "no_defects"] {
            cases.push(case(op, vec![t.clone()]));
        }
        cases.push(case("findings", vec![json!("f"), t.clone()]));
        for rx in ["HEAD", "SECTION", "VERDICT", "CUT"] {
            cases.push(case("finditer", vec![json!(rx), t.clone()]));
        }
        cases.push(case("check_reply", vec![plans[0].clone(), json!(0), t.clone()]));
    }
    eprintln!("corpus: {} texts", texts.len());
    let mut n = 0;
    for chunk in cases.chunks(4000) {
        gate("corpus", chunk);
        n += chunk.len();
    }
    eprintln!("corpus sweep: {} texts, {n} cases, 0 mismatches", texts.len());
}

#[test]
#[ignore]
fn random_sweep_matches_python() {
    let mut rng = Rng(0x5eed_5eed_5eed_5eed);
    let plans = checked_plans();
    let mut total: BTreeMap<&str, usize> = BTreeMap::new();
    for round in 0..20 {
        let mut cases = Vec::new();
        for _ in 0..2000 {
            let t = json!(text(&mut rng));
            for op in ["unfenced", "has_issues", "verdicts", "finding_id", "no_defects", "head_match"] {
                cases.push(case(op, vec![t.clone()]));
            }
            cases.push(case("findings", vec![json!("a"), t.clone()]));
            for rx in ["HEAD", "SECTION", "VERDICT", "CUT"] {
                cases.push(case("finditer", vec![json!(rx), t.clone()]));
            }
            cases.push(case("check_reply", vec![plans[0].clone(), json!(0), t.clone()]));
        }
        for _ in 0..300 {
            let plan = rng.pick(&plans).clone();
            let mut r = Replies::new();
            for id in ["a", "b", "r"] {
                if rng.chance(90) {
                    let n = rng.below(6);
                    r.insert(id.into(), review(&mut rng, n));
                }
            }
            let items = findings_items(&plan, &r);
            r.insert("k".into(), checker(&mut rng, &items));
            cases.push(case("assemble", vec![plan.clone(), json!(r)]));
            cases.push(case("requests", vec![plan.clone(), json!(plan["passes"].as_array().unwrap().len() - 1), json!(r)]));
        }
        *total.entry("cases").or_default() += cases.len();
        gate(&format!("random round {round}"), &cases);
    }
    let fx = Fixture::new("sweep");
    let mut cases = Vec::new();
    for _ in 0..20000 {
        let plan = random_plan(&mut rng);
        cases.push(case("validate", vec![plan, json!(fx.path("prompts"))]));
    }
    *total.entry("cases").or_default() += cases.len();
    gate("random validate", &cases);
    eprintln!("random sweep: {total:?}");
}
