#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_celve-cli"))
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().expect("run celve-cli")
}

fn ok(args: &[&str]) -> String {
    let o = run(args);
    assert!(o.status.success(), "celve-cli {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("celve-cli-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn version_and_usage() {
    assert_eq!(ok(&["--version"]).trim(), format!("celve-cli {}", env!("CARGO_PKG_VERSION")));
    assert_eq!(ok(&["version"]).trim(), format!("celve-cli {}", env!("CARGO_PKG_VERSION")));
    let o = run(&["frobnicate"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("unknown subcommand"));
    let o = run(&[]);
    assert!(!o.status.success());
}

#[test]
fn eval_formulas() {
    assert_eq!(ok(&["eval", "=SUM(1,2,3)"]).trim(), "6");
    assert_eq!(ok(&["eval", "1/4"]).trim(), "0.25");
    assert_eq!(ok(&["eval", "=UPPER(\"abc\")"]).trim(), "ABC");
    assert_eq!(ok(&["eval", "=1/0"]).trim(), "#DIV/0!");
    assert_eq!(ok(&["eval", "=2>1", "--json"]).trim(), "true");
    let o = run(&["eval", "=1", "--in", "/no/such/file.xlsx"]);
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stderr).is_empty());
}

#[test]
fn run_then_cat_and_info_on_sample() {
    let dir = tmpdir("run");
    let book = dir.join("budget.xlsx");
    let book_s = book.to_str().unwrap();
    let script = dir.join("steps.jsonl");
    std::fs::write(
        &script,
        "# steps\n{\"command\":\"cell.set\",\"params\":{\"cell\":\"K2\",\"input\":\"=K1*2\"}}\nhome.bold {\"range\":\"K1:K2\"}\n",
    )
    .unwrap();
    let out = ok(&[
        "run",
        "--sample",
        "budget",
        "--cmd",
        r#"cell.set={"cell":"K1","input":"21"}"#,
        "--script",
        script.to_str().unwrap(),
        "--out",
        book_s,
        "--print",
        "K1:K2",
        "--quiet",
    ]);
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["21", "42"], "{out}");
    assert!(book.exists());

    let table = ok(&["cat", book_s, "--range", "K1:K2"]);
    assert_eq!(table.split_whitespace().collect::<Vec<_>>(), ["21", "42"]);
    let formulas = ok(&["cat", book_s, "--range", "K2", "--formulas"]);
    assert_eq!(formulas.trim(), "=K1*2");
    let whole = ok(&["cat", book_s]);
    assert!(whole.lines().count() > 3, "{whole}");

    let info = ok(&["info", book_s]);
    assert!(info.contains("Budget"), "{info}");
    let json: serde_json::Value = serde_json::from_str(&ok(&["info", book_s, "--json"])).unwrap();
    assert!(json["sheets"][0]["cells"].as_u64().unwrap() > 10);

    // A failing command gives a non-zero exit and names the step.
    let o = run(&["run", "--cmd", "no.such.command"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("no.such.command"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn convert_xlsx_csv_roundtrip() {
    let dir = tmpdir("convert");
    let xlsx = dir.join("in.xlsx");
    let csv = dir.join("out.csv");
    let back = dir.join("back.xlsx");
    ok(&[
        "run",
        "--cmd",
        r#"range.setValues={"range":"A1","values":[["Name","Qty"],["apple",3],["pear, green",4],["total","=SUM(B2:B3)"]]}"#,
        "--out",
        xlsx.to_str().unwrap(),
        "--quiet",
    ]);
    ok(&["convert", xlsx.to_str().unwrap(), csv.to_str().unwrap()]);
    let text = std::fs::read_to_string(&csv).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, ["Name,Qty", "apple,3", "\"pear, green\",4", "total,7"], "{text}");
    ok(&["convert", csv.to_str().unwrap(), back.to_str().unwrap()]);
    let again = ok(&["cat", back.to_str().unwrap(), "--csv"]);
    assert_eq!(again.lines().collect::<Vec<_>>(), lines);
    let o = run(&["convert", xlsx.to_str().unwrap(), dir.join("x.unknown").to_str().unwrap()]);
    assert!(!o.status.success());
    let _ = std::fs::remove_dir_all(dir);
}

/// A minimal Excel 97-2003 workbook, assembled from the public [MS-XLS] and [MS-CFB] layouts:
/// sheet "Data" with A1 = 21, B1 = A1*2 (saved result 42) and A2 = "hi".
fn tiny_xls() -> Vec<u8> {
    fn rec(kind: u16, body: &[u8]) -> Vec<u8> {
        [kind.to_le_bytes().to_vec(), (body.len() as u16).to_le_bytes().to_vec(), body.to_vec()].concat()
    }
    let bof = |dt: u16| rec(0x0809, &[0x0600u16.to_le_bytes().to_vec(), dt.to_le_bytes().to_vec(), vec![0; 12]].concat());
    let eof = rec(0x000A, &[]);
    let cell = |row: u16, col: u16| [row.to_le_bytes(), col.to_le_bytes(), 0u16.to_le_bytes()].concat();
    let mut sheet = bof(0x0010);
    sheet.extend(rec(0x0203, &[cell(0, 0), 21f64.to_le_bytes().to_vec()].concat()));
    // B1: PtgRef A1 (relative), PtgInt 2, PtgMul.
    let rgce = [0x24u8, 0, 0, 0, 0xC0, 0x1E, 2, 0, 0x05];
    sheet.extend(rec(
        0x0006,
        &[cell(0, 1), 42f64.to_le_bytes().to_vec(), vec![0; 6], (rgce.len() as u16).to_le_bytes().to_vec(), rgce.to_vec()].concat(),
    ));
    sheet.extend(rec(0x0204, &[cell(1, 0), 2u16.to_le_bytes().to_vec(), vec![0], b"hi".to_vec()].concat()));
    sheet.extend(&eof);
    let globals =
        |offset: u32| [bof(0x0005), rec(0x0085, &[offset.to_le_bytes().to_vec(), vec![0, 0, 4, 0], b"Data".to_vec()].concat()), eof.clone()].concat();
    let mut stream = globals(globals(0).len() as u32);
    stream.extend(sheet);
    stream.resize(4096, 0);
    // Compound file: the stream in sectors 0-7, the directory in 8, the FAT in 9.
    let n = stream.len() / 512;
    let mut out = vec![0u8; 512];
    out[..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    out[26] = 3;
    out[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
    out[30] = 9;
    out[32] = 6;
    out[44] = 1;
    out[48..52].copy_from_slice(&(n as u32).to_le_bytes());
    out[56..60].copy_from_slice(&4096u32.to_le_bytes());
    out[60..64].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
    out[68..72].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
    for i in 0..109 {
        let v = if i == 0 { n as u32 + 1 } else { 0xFFFF_FFFF };
        out[76 + i * 4..80 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    out.extend(&stream);
    let mut dir = vec![0u8; 512];
    for (k, (name, kind, child, start, size)) in
        [("Root Entry", 5u8, 1u32, 0xFFFF_FFFEu32, 0u32), ("Workbook", 2, 0xFFFF_FFFF, 0, stream.len() as u32)].into_iter().enumerate()
    {
        let e = &mut dir[k * 128..(k + 1) * 128];
        for (i, u) in name.encode_utf16().enumerate() {
            e[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        e[64..66].copy_from_slice(&((name.len() as u16 + 1) * 2).to_le_bytes());
        e[66] = kind;
        e[68..80].copy_from_slice(&[0xFF; 12]);
        e[76..80].copy_from_slice(&child.to_le_bytes());
        e[116..120].copy_from_slice(&start.to_le_bytes());
        e[120..124].copy_from_slice(&size.to_le_bytes());
    }
    out.extend(dir);
    let mut fat: Vec<u32> = (1..=n as u32).collect();
    fat[n - 1] = 0xFFFF_FFFE;
    fat.push(0xFFFF_FFFE);
    fat.push(0xFFFF_FFFD);
    fat.resize(128, 0xFFFF_FFFF);
    out.extend(fat.iter().flat_map(|v| v.to_le_bytes()));
    out
}

#[test]
fn xls_imports_and_converts_to_xlsx() {
    let dir = tmpdir("xls");
    let xls = dir.join("legacy.xls");
    let xlsx = dir.join("legacy.xlsx");
    std::fs::write(&xls, tiny_xls()).unwrap();
    let info = ok(&["info", xls.to_str().unwrap()]);
    assert!(info.contains("Data"), "{info}");
    ok(&["convert", xls.to_str().unwrap(), xlsx.to_str().unwrap()]);
    let text = ok(&["cat", xlsx.to_str().unwrap(), "--csv"]);
    assert_eq!(text.lines().collect::<Vec<_>>(), ["21,42", "hi,"], "{text}");
    let formulas = ok(&["cat", xlsx.to_str().unwrap(), "--formulas", "--csv"]);
    assert!(formulas.contains("=A1*2"), "{formulas}");
    assert!(ok(&["eval", "=B1+1", "--in", xls.to_str().unwrap()]).trim() == "43");
    // .xls is import-only.
    assert!(!run(&["convert", xlsx.to_str().unwrap(), dir.join("again.xls").to_str().unwrap()]).status.success());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn commands_and_functions() {
    let out = ok(&["commands", "--search", "bold"]);
    assert!(out.contains("home.bold"));
    let v: serde_json::Value = serde_json::from_str(&ok(&["commands", "--json"])).unwrap();
    assert!(v.as_array().unwrap().len() > 100);
    assert!(ok(&["functions", "--search", "vlookup"]).contains("VLOOKUP"));
}

#[test]
fn mcp_over_stdio() {
    let mut child = bin().arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"evaluate_formula","arguments":{{"formula":"=6*7"}}}}}}"#
        )
        .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "celve");
    assert!(lines[1]["result"]["content"][0]["text"].as_str().unwrap().contains("42"));
}
