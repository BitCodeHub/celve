//! Excel 97-2003 (.xls) import. Every fixture is an original BIFF8 record stream inside an
//! original compound file, assembled here from the public [MS-XLS] and [MS-CFB] layouts.
use gridcraft_core::{CellError, CellRef, DateSystem, RangeRef, Value};
use gridcraft_model::{BorderStyle, Color, HAlign, PatternType, Underline, Visibility, Workbook};

use crate::{Format, read_xls, read_xlsx, sniff, write_xlsx};

// ------------------------------------------------------------------ compound file writer

const END: u32 = 0xFFFF_FFFE;
const FREE: u32 = 0xFFFF_FFFF;
const NOSTREAM: u32 = 0xFFFF_FFFF;

/// A version-3 compound file (512-byte sectors) holding the given root-level streams. Streams
/// under 4096 bytes go to the mini stream, as the format requires.
pub fn cfb(streams: &[(&str, &[u8])]) -> Vec<u8> {
    let mut sectors: Vec<[u8; 512]> = vec![];
    let mut chains: Vec<(u32, u32)> = vec![];
    let mut alloc = |data: &[u8], sectors: &mut Vec<[u8; 512]>| -> u32 {
        let start = sectors.len() as u32;
        for chunk in data.chunks(512) {
            let mut s = [0u8; 512];
            s[..chunk.len()].copy_from_slice(chunk);
            sectors.push(s);
        }
        chains.push((start, data.len().div_ceil(512) as u32));
        start
    };
    let mut mini = vec![];
    let mut mini_fat: Vec<u32> = vec![];
    let mut starts = vec![];
    for (_, data) in streams {
        if data.len() < 4096 {
            let first = (mini.len() / 64) as u32;
            let n = data.len().div_ceil(64).max(1) as u32;
            for k in 0..n {
                mini_fat.push(if k + 1 == n { END } else { first + k + 1 });
            }
            mini.extend_from_slice(data);
            mini.resize(((first + n) * 64) as usize, 0);
            starts.push(first);
        } else {
            starts.push(alloc(data, &mut sectors));
        }
    }
    let mini_start = if mini.is_empty() { END } else { alloc(&mini, &mut sectors) };
    let mini_fat_bytes: Vec<u8> = mini_fat.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mini_fat_start = if mini_fat.is_empty() { END } else { alloc(&mini_fat_bytes, &mut sectors) };
    let mini_fat_sectors = mini_fat_bytes.len().div_ceil(512) as u32;
    // Directory: the root, then the streams chained as right siblings.
    let mut dir = vec![];
    let entry = |name: &str, kind: u8, right: u32, child: u32, start: u32, size: u32| {
        let mut e = [0u8; 128];
        let units: Vec<u16> = name.encode_utf16().collect();
        for (i, u) in units.iter().enumerate() {
            e[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        e[64..66].copy_from_slice(&((units.len() as u16 + 1) * 2).to_le_bytes());
        e[66] = kind;
        e[67] = 1;
        e[68..72].copy_from_slice(&NOSTREAM.to_le_bytes());
        e[72..76].copy_from_slice(&right.to_le_bytes());
        e[76..80].copy_from_slice(&child.to_le_bytes());
        e[116..120].copy_from_slice(&start.to_le_bytes());
        e[120..124].copy_from_slice(&size.to_le_bytes());
        e
    };
    dir.extend(entry("Root Entry", 5, NOSTREAM, if streams.is_empty() { NOSTREAM } else { 1 }, mini_start, mini.len() as u32));
    for (i, (name, data)) in streams.iter().enumerate() {
        let right = if i + 1 < streams.len() { i as u32 + 2 } else { NOSTREAM };
        dir.extend(entry(name, 2, right, NOSTREAM, starts[i], data.len() as u32));
    }
    let dir_start = alloc(&dir, &mut sectors);
    // FAT sectors go last.
    let content = sectors.len() as u32;
    let mut fat_n = 1u32;
    while content + fat_n > fat_n * 128 {
        fat_n += 1;
    }
    let mut fat = vec![FREE; (fat_n * 128) as usize];
    for (start, n) in chains {
        for k in 0..n {
            fat[(start + k) as usize] = if k + 1 == n { END } else { start + k + 1 };
        }
    }
    for k in 0..fat_n {
        fat[(content + k) as usize] = 0xFFFF_FFFD;
    }
    let mut out = vec![0u8; 512];
    out[..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    out[24..26].copy_from_slice(&0x3Eu16.to_le_bytes());
    out[26..28].copy_from_slice(&3u16.to_le_bytes());
    out[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
    out[30..32].copy_from_slice(&9u16.to_le_bytes());
    out[32..34].copy_from_slice(&6u16.to_le_bytes());
    out[44..48].copy_from_slice(&fat_n.to_le_bytes());
    out[48..52].copy_from_slice(&dir_start.to_le_bytes());
    out[56..60].copy_from_slice(&4096u32.to_le_bytes());
    out[60..64].copy_from_slice(&mini_fat_start.to_le_bytes());
    out[64..68].copy_from_slice(&mini_fat_sectors.to_le_bytes());
    out[68..72].copy_from_slice(&END.to_le_bytes());
    for i in 0..109 {
        let v = if (i as u32) < fat_n { content + i as u32 } else { FREE };
        out[76 + i * 4..80 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    for s in &sectors {
        out.extend_from_slice(s);
    }
    for v in fat {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

// ------------------------------------------------------------------ BIFF8 writer

fn rec(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut v = kind.to_le_bytes().to_vec();
    v.extend((body.len() as u16).to_le_bytes());
    v.extend(body);
    v
}

fn bof(dt: u16) -> Vec<u8> {
    let mut b = 0x0600u16.to_le_bytes().to_vec();
    b.extend(dt.to_le_bytes());
    b.extend([0u8; 12]);
    rec(0x0809, &b)
}

fn eof() -> Vec<u8> {
    rec(0x000A, &[])
}

fn chars(s: &str) -> (u8, Vec<u8>) {
    if s.chars().all(|c| (c as u32) < 256) {
        (0, s.chars().map(|c| c as u8).collect())
    } else {
        (1, s.encode_utf16().flat_map(u16::to_le_bytes).collect())
    }
}

fn xl_str(s: &str) -> Vec<u8> {
    let (flag, data) = chars(s);
    let mut v = (s.encode_utf16().count() as u16).to_le_bytes().to_vec();
    v.push(flag);
    v.extend(data);
    v
}

fn short_str(s: &str) -> Vec<u8> {
    let (flag, data) = chars(s);
    let mut v = vec![s.encode_utf16().count() as u8, flag];
    v.extend(data);
    v
}

fn cell(row: u16, col: u16, xf: u16) -> Vec<u8> {
    [row.to_le_bytes(), col.to_le_bytes(), xf.to_le_bytes()].concat()
}

fn number(row: u16, col: u16, xf: u16, n: f64) -> Vec<u8> {
    rec(0x0203, &[cell(row, col, xf), n.to_le_bytes().to_vec()].concat())
}

fn rk(row: u16, col: u16, xf: u16, v: u32) -> Vec<u8> {
    rec(0x027E, &[cell(row, col, xf), v.to_le_bytes().to_vec()].concat())
}

fn rk_int(n: i32) -> u32 {
    ((n << 2) as u32) | 2
}

/// A FORMULA record: cached value bytes, then the parsed expression and its extra data.
fn formula(row: u16, col: u16, xf: u16, cached: [u8; 8], rgce: &[u8], rgcb: &[u8]) -> Vec<u8> {
    let mut b = cell(row, col, xf);
    b.extend(cached);
    b.extend(0u16.to_le_bytes());
    b.extend(0u32.to_le_bytes());
    b.extend((rgce.len() as u16).to_le_bytes());
    b.extend(rgce);
    b.extend(rgcb);
    rec(0x0006, &b)
}

fn num(n: f64) -> [u8; 8] {
    n.to_le_bytes()
}

fn special(kind: u8, v: u8) -> [u8; 8] {
    [kind, 0, v, 0, 0, 0, 0xFF, 0xFF]
}

fn ptg_ref(t: u8, row: u16, col: u16, row_rel: bool, col_rel: bool) -> Vec<u8> {
    let c = col | if col_rel { 0x4000 } else { 0 } | if row_rel { 0x8000 } else { 0 };
    [vec![t], row.to_le_bytes().to_vec(), c.to_le_bytes().to_vec()].concat()
}

fn ptg_area(t: u8, r0: u16, r1: u16, c0: u16, c1: u16) -> Vec<u8> {
    let rel = |c: u16| c | 0xC000;
    [vec![t], r0.to_le_bytes().to_vec(), r1.to_le_bytes().to_vec(), rel(c0).to_le_bytes().to_vec(), rel(c1).to_le_bytes().to_vec()].concat()
}

fn ptg_int(n: u16) -> Vec<u8> {
    [vec![0x1E], n.to_le_bytes().to_vec()].concat()
}

fn func_var(argc: u8, id: u16) -> Vec<u8> {
    [vec![0x42, argc], id.to_le_bytes().to_vec()].concat()
}

fn xf(font: u16, fmt: u16, style: bool, align: u8, border1: u32, border2: u32, fill: u16) -> Vec<u8> {
    let mut b = font.to_le_bytes().to_vec();
    b.extend(fmt.to_le_bytes());
    b.extend((if style { 0xFFF5u16 } else { 0x0001 }).to_le_bytes());
    b.extend([align, 0, 0, 0]);
    b.extend(border1.to_le_bytes());
    b.extend(border2.to_le_bytes());
    b.extend(fill.to_le_bytes());
    rec(0x00E0, &b)
}

fn font(twips: u16, italic: bool, icv: u16, bold: bool, underline: u8, name: &str) -> Vec<u8> {
    let mut b = twips.to_le_bytes().to_vec();
    b.extend((if italic { 2u16 } else { 0 }).to_le_bytes());
    b.extend(icv.to_le_bytes());
    b.extend((if bold { 700u16 } else { 400 }).to_le_bytes());
    b.extend(0u16.to_le_bytes());
    b.extend([underline, 0, 0, 0]);
    b.extend(short_str(name));
    rec(0x0031, &b)
}

fn boundsheet(offset: u32, state: u8, kind: u8, name: &str) -> Vec<u8> {
    rec(0x0085, &[offset.to_le_bytes().to_vec(), vec![state, kind], short_str(name)].concat())
}

/// A workbook stream: the globals (without BOUNDSHEET records or EOF) plus the sheets, each
/// `(name, visibility state, BoundSheet type, substream)`. Sheet offsets are filled in here.
fn workbook(globals: &[u8], sheets: &[(&str, u8, u8, Vec<u8>)]) -> Vec<u8> {
    let head = |offsets: &[u32]| -> Vec<u8> {
        let mut g = bof(0x0005);
        g.extend(globals);
        for ((name, state, kind, _), off) in sheets.iter().zip(offsets) {
            g.extend(boundsheet(*off, *state, *kind, name));
        }
        g.extend(eof());
        g
    };
    let len = head(&vec![0; sheets.len()]).len() as u32;
    let mut offsets = vec![];
    let mut at = len;
    for (_, _, _, s) in sheets {
        offsets.push(at);
        at += s.len() as u32;
    }
    let mut out = head(&offsets);
    for (_, _, _, s) in sheets {
        out.extend(s);
    }
    out
}

fn sheet(body: &[u8]) -> Vec<u8> {
    [bof(0x0010), body.to_vec(), eof()].concat()
}

/// Pads a workbook stream past the mini-stream cutoff, as Excel does.
fn padded(mut stream: Vec<u8>) -> Vec<u8> {
    if stream.len() < 4096 {
        stream.resize(4096, 0);
    }
    stream
}

fn xls(stream: &[u8]) -> Vec<u8> {
    cfb(&[("Workbook", stream)])
}

fn simple(body: &[u8]) -> Vec<u8> {
    xls(&workbook(&[], &[("Sheet1", 0, 0, sheet(body))]))
}

// ------------------------------------------------------------------ the full fixture

fn rich_fixture() -> Vec<u8> {
    let mut g = vec![];
    g.extend(rec(0x0042, &1200u16.to_le_bytes()));
    // WINDOW1: active tab 0.
    g.extend(rec(0x003D, &[0u8; 18]));
    g.extend(rec(0x0022, &0u16.to_le_bytes()));
    g.extend(font(200, false, 0x7FFF, false, 0, "Arial"));
    g.extend(font(200, false, 0x7FFF, true, 0, "Arial"));
    g.extend(font(200, true, 0x7FFF, false, 0, "Arial"));
    g.extend(font(200, false, 0x7FFF, false, 0, "Arial"));
    // Font index 4 is never written; this one is index 5.
    g.extend(font(240, false, 10, false, 1, "Courier New"));
    g.extend(rec(0x041E, &[164u16.to_le_bytes().to_vec(), xl_str("0.000")].concat()));
    g.extend(rec(0x041E, &[165u16.to_le_bytes().to_vec(), xl_str("yyyy-mm-dd")].concat()));
    for _ in 0..15 {
        g.extend(xf(0, 0, true, 0x20, 0, 0, 0));
    }
    g.extend(xf(0, 0, false, 0x20, 0, 0, 0)); // 15 default
    g.extend(xf(1, 0, false, 0x20, 0, 0, 0)); // 16 bold
    g.extend(xf(0, 164, false, 0x20, 0, 0, 0)); // 17 0.000
    g.extend(xf(0, 165, false, 0x20, 0, 0, 0)); // 18 date
    // 19: centred, wrapped, thin red bottom border, solid fill of colour 13.
    g.extend(xf(0, 0, false, 0x02 | 0x08 | 0x20, 1 << 12, (10 << 7) | (1 << 26), 13 | (64 << 7)));
    g.extend(xf(5, 10, false, 0x20, 0, 0, 0)); // 20 Courier red underlined, 0.00%
    // PALETTE: colour 13 (entry 5) becomes #112233.
    let mut pal = 56u16.to_le_bytes().to_vec();
    for i in 0..56usize {
        let v = if i == 5 { 0x112233 } else { crate::tables::INDEXED[8 + i] };
        pal.extend([(v >> 16) as u8, (v >> 8) as u8, v as u8, 0]);
    }
    g.extend(rec(0x0092, &pal));
    // SST: a split string continues in UTF-16 after a fresh flag byte; a rich string with
    // runs and phonetic data; a string that starts in the CONTINUE record.
    let mut sst = 6u32.to_le_bytes().to_vec();
    sst.extend(6u32.to_le_bytes());
    sst.extend(xl_str("Hello"));
    sst.extend(xl_str("Wörld"));
    sst.extend(xl_str("日本語"));
    sst.extend([9u8, 0, 0]);
    sst.extend(b"abcdef");
    let mut cont = vec![1u8];
    cont.extend("XYZ".encode_utf16().flat_map(u16::to_le_bytes));
    // Rich: 2 runs, 4 bytes of phonetic data.
    cont.extend(4u16.to_le_bytes());
    cont.push(0x0C);
    cont.extend(2u16.to_le_bytes());
    cont.extend(4u32.to_le_bytes());
    cont.extend(b"rich");
    cont.extend([0u8; 8]);
    cont.extend([0u8; 4]);
    cont.extend(xl_str("next"));
    g.extend(rec(0x00FC, &sst));
    g.extend(rec(0x003C, &cont));
    // Links: this workbook (two sheets), then the add-in functions.
    g.extend(rec(0x01AE, &[3u16.to_le_bytes(), 0x0401u16.to_le_bytes()].concat()));
    g.extend(rec(0x01AE, &[1u16.to_le_bytes(), 0x3A01u16.to_le_bytes()].concat()));
    g.extend(rec(0x0023, &[vec![0u8; 6], short_str("_xlfn.IFERROR"), vec![1, 0, 0x17]].concat()));
    let mut xti = 4u16.to_le_bytes().to_vec();
    for (s, a, b) in [(0u16, 0i16, 0i16), (0, 1, 1), (1, -2, -2), (0, 0, 1)] {
        xti.extend(s.to_le_bytes());
        xti.extend(a.to_le_bytes());
        xti.extend(b.to_le_bytes());
    }
    g.extend(rec(0x0017, &xti));
    // NAME TaxRate = 'My Sheet'!$A$1 (workbook scope).
    let rgce = [vec![0x3A], 1u16.to_le_bytes().to_vec(), 0u16.to_le_bytes().to_vec(), 0u16.to_le_bytes().to_vec()].concat();
    let mut name = 0u16.to_le_bytes().to_vec();
    name.extend([0, 7]);
    name.extend((rgce.len() as u16).to_le_bytes());
    name.extend([0, 0]);
    name.extend(0u16.to_le_bytes());
    name.extend([0, 0, 0, 0]);
    name.push(0);
    name.extend(b"TaxRate");
    name.extend(&rgce);
    g.extend(rec(0x0018, &name));
    // A built-in Print_Area on sheet 1: skipped with a warning.
    let pa = [vec![0x3B], 0u16.to_le_bytes().to_vec(), [0u8; 8].to_vec()].concat();
    let mut name = 0x20u16.to_le_bytes().to_vec();
    name.extend([0, 1]);
    name.extend((pa.len() as u16).to_le_bytes());
    name.extend([0, 0]);
    name.extend(1u16.to_le_bytes());
    name.extend([0, 0, 0, 0, 0, 0x06]);
    name.extend(&pa);
    g.extend(rec(0x0018, &name));

    let mut s = vec![];
    s.extend(rec(0x0055, &8u16.to_le_bytes()));
    s.extend(rec(0x0225, &[0u16.to_le_bytes(), 255u16.to_le_bytes()].concat()));
    // Columns B:C 20 characters wide, F hidden.
    s.extend(rec(0x007D, &[1u16, 2, 20 * 256, 15, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>()));
    s.extend(rec(0x007D, &[5u16, 5, 2340, 15, 1, 0].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>()));
    // Row 3 is 30 pt tall (custom), row 5 hidden.
    let row = |r: u16, height: u16, flags: u32| {
        rec(0x0208, &[r.to_le_bytes().to_vec(), vec![0; 4], height.to_le_bytes().to_vec(), vec![0; 4], flags.to_le_bytes().to_vec()].concat())
    };
    s.extend(row(2, 600, 0x40 | (15 << 16)));
    s.extend(row(4, 255, 0x20 | (15 << 16)));
    s.extend(number(0, 0, 15, 1.5));
    s.extend(rk(0, 1, 16, rk_int(42)));
    s.extend(rk(0, 2, 17, rk_int(1234) | 1));
    s.extend(rk(0, 3, 15, 0x3FE0_0000));
    s.extend(rk(0, 4, 20, rk_int(-7)));
    // MULRK row 2: 1, 2, 3.
    let mut m = [1u16.to_le_bytes(), 0u16.to_le_bytes()].concat();
    for n in 1..=3 {
        m.extend(15u16.to_le_bytes());
        m.extend(rk_int(n).to_le_bytes());
    }
    m.extend(2u16.to_le_bytes());
    s.extend(rec(0x00BD, &m));
    let sst_cell = |row: u16, col: u16, i: u32| rec(0x00FD, &[cell(row, col, 15), i.to_le_bytes().to_vec()].concat());
    s.extend(sst_cell(2, 0, 0));
    s.extend(sst_cell(2, 1, 3));
    s.extend(rec(0x0204, &[cell(2, 2, 15), xl_str("inline")].concat()));
    s.extend(sst_cell(2, 3, 2));
    s.extend(sst_cell(2, 4, 4));
    s.extend(sst_cell(2, 5, 5));
    s.extend(sst_cell(2, 6, 1));
    s.extend(rec(0x0205, &[cell(3, 0, 15), vec![1, 0]].concat()));
    s.extend(rec(0x0205, &[cell(3, 1, 15), vec![0x07, 1]].concat()));
    s.extend(rec(0x0201, &cell(4, 0, 19)));
    s.extend(rec(0x0201, &cell(4, 1, 15)));
    s.extend(rec(0x00BE, &[4u16, 2, 19, 19, 3].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>()));
    // A6 =A1+B1*2
    let f = [ptg_ref(0x24, 0, 0, true, true), ptg_ref(0x24, 0, 1, true, true), ptg_int(2), vec![0x05, 0x03]].concat();
    s.extend(formula(5, 0, 15, num(85.5), &f, &[]));
    // B6 =SUM(A2:C2) through tAttrSum.
    let f = [ptg_area(0x25, 1, 1, 0, 2), vec![0x19, 0x10, 0, 0]].concat();
    s.extend(formula(5, 1, 15, num(6.0), &f, &[]));
    // C6 =IF(A1>1,"big","small") with the IF/GOTO jump attributes; the result is a string.
    let f = [
        ptg_ref(0x44, 0, 0, true, true),
        ptg_int(1),
        vec![0x0D],
        vec![0x19, 0x02, 9, 0],
        vec![0x17, 3, 0],
        b"big".to_vec(),
        vec![0x19, 0x08, 10, 0],
        vec![0x17, 5, 0],
        b"small".to_vec(),
        vec![0x19, 0x08, 3, 0],
        func_var(3, 1),
    ]
    .concat();
    s.extend(formula(5, 2, 15, special(0, 0), &f, &[]));
    s.extend(rec(0x0207, &xl_str("big")));
    // D6 ='My Sheet'!B2+SUM('Data:My Sheet'!A1:A1)
    let r3 = [vec![0x5A], 1u16.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), (1u16 | 0xC000).to_le_bytes().to_vec()].concat();
    let a3 = [vec![0x3B], 3u16.to_le_bytes().to_vec(), [0u8, 0, 0, 0, 0, 0xC0, 0, 0xC0].to_vec()].concat();
    let f = [r3, a3, func_var(1, 4), vec![0x03]].concat();
    s.extend(formula(5, 3, 15, num(5.07), &f, &[]));
    // E6 =IFERROR(1/0,"x"): an add-in name called through function 255.
    let f = [
        vec![0x39],
        2u16.to_le_bytes().to_vec(),
        1u32.to_le_bytes().to_vec(),
        ptg_int(1),
        ptg_int(0),
        vec![0x06],
        vec![0x17, 1, 0, b'x'],
        func_var(3, 255),
    ]
    .concat();
    s.extend(formula(5, 4, 15, special(0, 0), &f, &[]));
    s.extend(rec(0x0207, &xl_str("x")));
    // F6 =TaxRate*2, G6 =TRUE(), H6 =1/0
    let f = [vec![0x43], 1u32.to_le_bytes().to_vec(), ptg_int(2), vec![0x05]].concat();
    s.extend(formula(5, 5, 15, num(0.14), &f, &[]));
    s.extend(formula(5, 6, 15, special(1, 1), &[0x41, 34, 0], &[]));
    s.extend(formula(5, 7, 15, special(2, 0x07), &[ptg_int(1), ptg_int(0), vec![0x06]].concat(), &[]));
    // A7 =SUM({1,2;3,4})
    let mut extra = vec![1u8];
    extra.extend(1u16.to_le_bytes());
    for n in [1.0f64, 2.0, 3.0, 4.0] {
        extra.push(1);
        extra.extend(n.to_le_bytes());
    }
    s.extend(formula(6, 0, 15, num(10.0), &[vec![0x60], vec![0; 7], func_var(1, 4)].concat(), &extra));
    // A8 =-A1%
    s.extend(formula(7, 0, 15, num(-0.015), &[ptg_ref(0x44, 0, 0, true, true), vec![0x14, 0x13]].concat(), &[]));
    // A9:A11 share =A1*10 (relative to each cell).
    let exp = |r: u16, c: u16| [vec![0x01], r.to_le_bytes().to_vec(), c.to_le_bytes().to_vec()].concat();
    s.extend(formula(8, 0, 15, num(15.0), &exp(8, 0), &[]));
    let shr = [ptg_ref(0x2C, (-8i16) as u16, 0, true, true), ptg_int(10), vec![0x05]].concat();
    let mut body = [8u16.to_le_bytes(), 10u16.to_le_bytes()].concat();
    body.extend([0, 0, 0, 3]);
    body.extend((shr.len() as u16).to_le_bytes());
    body.extend(&shr);
    s.extend(rec(0x04BC, &body));
    s.extend(formula(9, 0, 15, num(10.0), &exp(8, 0), &[]));
    s.extend(formula(10, 0, 15, num(0.0), &exp(8, 0), &[]));
    // B9:B10 {=A9:A10*2}
    s.extend(formula(8, 1, 15, num(30.0), &exp(8, 1), &[]));
    let arr = [ptg_area(0x65, 8, 9, 0, 0), ptg_int(2), vec![0x05]].concat();
    let mut body = [8u16.to_le_bytes(), 9u16.to_le_bytes()].concat();
    body.extend([1, 1]);
    body.extend([0u8; 6]);
    body.extend((arr.len() as u16).to_le_bytes());
    body.extend(&arr);
    s.extend(rec(0x0221, &body));
    s.extend(formula(9, 1, 15, num(20.0), &exp(8, 1), &[]));
    // C9: a token GridCraft doesn't read keeps its value.
    s.extend(formula(8, 2, 15, num(99.0), &[0x18, 0x01, 0, 0], &[]));
    // D9 =SUM(A:A)
    s.extend(formula(8, 3, 15, num(123.0), &[ptg_area(0x25, 0, 0xFFFF, 0, 0), func_var(1, 4)].concat(), &[]));
    s.extend(number(8, 4, 18, 45000.0));
    s.extend(rec(0x00E5, &[1u16, 11, 12, 0, 1].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>()));
    s.extend(rec(0x023E, &[(0x02u16 | 0x04 | 0x08 | 0x10 | 0x20).to_le_bytes(), 0u16.to_le_bytes(), 0u16.to_le_bytes()].concat()));
    s.extend(rec(0x0041, &[1u16, 2, 2, 1, 0].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>()));
    s.extend(rec(0x00A0, &[3u16.to_le_bytes(), 2u16.to_le_bytes()].concat()));
    s.extend(rec(0x01B8, &[0u8; 32]));
    s.extend(rec(0x001C, &[0u8; 12]));
    // An embedded chart's substream: its records never reach the sheet.
    s.extend(bof(0x0020));
    s.extend(number(50, 0, 15, 666.0));
    s.extend(eof());

    let my = sheet(&[number(0, 0, 15, 0.07), number(1, 1, 15, 5.0)].concat());
    let chart = [bof(0x0020), eof()].concat();
    xls(&padded(workbook(&g, &[("Data", 0, 0, sheet(&s)), ("My Sheet", 1, 0, my), ("Chart1", 0, 2, chart)])))
}

fn at(a1: &str) -> CellRef {
    CellRef::parse(a1).unwrap()
}

fn formula_text(wb: &Workbook, a1: &str) -> Option<String> {
    wb.sheets[0].cells.get(at(a1)).and_then(|c| c.formula.as_ref()).map(|f| f.text.clone())
}

fn value(wb: &Workbook, a1: &str) -> Value {
    wb.sheets[0].value(at(a1))
}

#[test]
fn xls_values_strings_formulas_styles_and_layout_import() {
    let bytes = rich_fixture();
    assert_eq!(sniff(&bytes), Format::Xls);
    let (wb, report) = read_xls(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 2, "the chart sheet is skipped");
    assert_eq!(wb.sheets[0].name, "Data");
    assert_eq!(wb.sheets[1].name, "My Sheet");
    assert_eq!(wb.sheets[1].visibility, Visibility::Hidden);
    assert_eq!(wb.active_sheet, 0);
    assert_eq!(wb.date_system, DateSystem::D1900);

    // Numbers: NUMBER, RK integer, RK ×100, RK double, negative RK, MULRK.
    assert_eq!(value(&wb, "A1"), Value::Number(1.5));
    assert_eq!(value(&wb, "B1"), Value::Number(42.0));
    assert_eq!(value(&wb, "C1"), Value::Number(12.34));
    assert_eq!(value(&wb, "D1"), Value::Number(0.5));
    assert_eq!(value(&wb, "E1"), Value::Number(-7.0));
    assert_eq!([value(&wb, "A2"), value(&wb, "B2"), value(&wb, "C2")], [Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)]);
    // Strings: shared, split across CONTINUE (switching to UTF-16), inline, UTF-16, rich, Latin-1.
    assert_eq!(value(&wb, "A3"), Value::text("Hello"));
    assert_eq!(value(&wb, "B3"), Value::text("abcdefXYZ"));
    assert_eq!(value(&wb, "C3"), Value::text("inline"));
    assert_eq!(value(&wb, "D3"), Value::text("日本語"));
    assert_eq!(value(&wb, "E3"), Value::text("rich"));
    assert_eq!(value(&wb, "F3"), Value::text("next"));
    assert_eq!(value(&wb, "G3"), Value::text("Wörld"));
    assert_eq!(value(&wb, "A4"), Value::Bool(true));
    assert_eq!(value(&wb, "B4"), Value::Error(CellError::Div0));

    // Styled blanks are kept, default ones aren't.
    assert!(wb.sheets[0].cells.get(at("A5")).is_some());
    assert!(wb.sheets[0].cells.get(at("B5")).is_none());
    assert!(wb.sheets[0].cells.get(at("C5")).is_some() && wb.sheets[0].cells.get(at("D5")).is_some());

    // Formulas keep their saved results and become formula text again.
    assert_eq!(formula_text(&wb, "A6").as_deref(), Some("A1+B1*2"));
    assert_eq!(value(&wb, "A6"), Value::Number(85.5));
    assert_eq!(formula_text(&wb, "B6").as_deref(), Some("SUM(A2:C2)"));
    assert_eq!(formula_text(&wb, "C6").as_deref(), Some("IF(A1>1,\"big\",\"small\")"));
    assert_eq!(value(&wb, "C6"), Value::text("big"));
    assert_eq!(formula_text(&wb, "D6").as_deref(), Some("'My Sheet'!B2+SUM('Data:My Sheet'!A1:A1)"));
    assert_eq!(formula_text(&wb, "E6").as_deref(), Some("IFERROR(1/0,\"x\")"));
    assert_eq!(value(&wb, "E6"), Value::text("x"));
    assert_eq!(formula_text(&wb, "F6").as_deref(), Some("TaxRate*2"));
    assert_eq!(formula_text(&wb, "G6").as_deref(), Some("TRUE()"));
    assert_eq!(value(&wb, "G6"), Value::Bool(true));
    assert_eq!(value(&wb, "H6"), Value::Error(CellError::Div0));
    assert_eq!(formula_text(&wb, "A7").as_deref(), Some("SUM({1,2;3,4})"));
    assert_eq!(formula_text(&wb, "A8").as_deref(), Some("-A1%"));
    // Shared formula, relative to each cell.
    assert_eq!(formula_text(&wb, "A9").as_deref(), Some("A1*10"));
    assert_eq!(formula_text(&wb, "A10").as_deref(), Some("A2*10"));
    assert_eq!(formula_text(&wb, "A11").as_deref(), Some("A3*10"));
    // Legacy array formula on its anchor; the other cells are filled by it.
    let b9 = wb.sheets[0].cells.get(at("B9")).unwrap().formula.clone().unwrap();
    assert_eq!(b9.text, "A9:A10*2");
    assert_eq!(b9.array, Some(RangeRef::parse("B9:B10").unwrap()));
    assert!(wb.sheets[0].cells.get(at("B10")).unwrap().formula.is_none());
    // An unreadable token keeps the value and says so.
    assert_eq!(formula_text(&wb, "C9"), None);
    assert_eq!(value(&wb, "C9"), Value::Number(99.0));
    assert!(report.warnings.iter().any(|w| w.contains("1 formula in this .xls file")), "{:?}", report.warnings);
    assert_eq!(formula_text(&wb, "D9").as_deref(), Some("SUM(A:A)"));
    // The embedded chart's NUMBER never landed on the sheet.
    assert!(wb.sheets[0].cells.get(at("A51")).is_none());

    // Defined names: TaxRate imported, the built-in print area skipped with a warning.
    assert_eq!(wb.names.len(), 1);
    assert_eq!(wb.names[0].name, "TaxRate");
    assert_eq!(wb.names[0].formula, "'My Sheet'!$A$1");
    assert_eq!(wb.names[0].scope, None);
    assert!(report.warnings.iter().any(|w| w.contains("Print areas")));

    // Styles.
    let style = |a1: &str| wb.styles.get(wb.sheets[0].style_id(at(a1))).clone();
    assert_eq!(style("A1").font.name, "Arial");
    assert_eq!(style("A1").font.size, 10.0);
    assert!(style("B1").font.bold);
    assert_eq!(style("C1").num_fmt.as_str(), "0.000");
    assert_eq!(style("E9").num_fmt.as_str(), "yyyy-mm-dd");
    let e1 = style("E1");
    assert_eq!(e1.num_fmt.as_str(), "0.00%");
    assert_eq!(e1.font.name, "Courier New");
    assert_eq!(e1.font.size, 12.0);
    assert_eq!(e1.font.underline, Underline::Single);
    assert_eq!(e1.font.color, Color::Rgb(0xFF0000));
    let a5 = style("A5");
    assert_eq!(a5.fill.pattern, PatternType::Solid);
    assert_eq!(a5.fill.fg, Color::Rgb(0x112233), "PALETTE overrides the default colours");
    assert_eq!(a5.borders.bottom.style, BorderStyle::Thin);
    assert_eq!(a5.borders.bottom.color, Color::Rgb(0xFF0000));
    assert_eq!(a5.align.h, HAlign::Center);
    assert!(a5.align.wrap);

    // Layout.
    let sh = &wb.sheets[0];
    assert_eq!(sh.merges, vec![RangeRef::parse("A12:B13").unwrap()]);
    assert!(sh.cols.get(&1).and_then(|c| c.size).is_some_and(|w| w > 100.0));
    assert_eq!(sh.cols.get(&1), sh.cols.get(&2));
    assert!(sh.cols.get(&5).is_some_and(|c| c.hidden));
    assert_eq!(sh.rows.get(&2).and_then(|r| r.size), Some(40.0));
    assert!(sh.rows.get(&4).is_some_and(|r| r.hidden));
    assert_eq!(sh.freeze, Some((2, 1)));
    assert_eq!(sh.zoom, 150);
    assert!(sh.show_gridlines);

    for w in ["Hyperlinks", "Comments", "Chart sheets", "Charts, pictures"] {
        assert!(report.warnings.iter().any(|x| x.contains(w)), "missing warning about {w}: {:?}", report.warnings);
    }

    // The workbook saves as .xlsx and reads back.
    let (back, _) = read_xlsx(&write_xlsx(&wb).unwrap()).unwrap();
    assert_eq!(back.sheets[0].value(at("B3")), Value::text("abcdefXYZ"));
    assert_eq!(back.sheets[0].cells.get(at("A10")).unwrap().formula.as_ref().unwrap().text, "A2*10");
    assert_eq!(back.names[0].formula, "'My Sheet'!$A$1");
}

#[test]
fn xls_small_workbook_in_the_mini_stream_with_1904_dates() {
    let g = rec(0x0022, &1u16.to_le_bytes());
    let stream = workbook(&g, &[("Dates", 0, 0, sheet(&number(0, 0, 15, 0.0)))]);
    assert!(stream.len() < 4096, "this fixture exercises the mini stream");
    let (wb, report) = read_xls(&xls(&stream)).unwrap();
    assert_eq!(wb.date_system, DateSystem::D1904);
    assert_eq!(wb.sheets[0].value(CellRef::new(0, 0)), Value::Number(0.0));
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn xls_password_older_versions_and_other_compound_files_are_refused_clearly() {
    let g = rec(0x002F, &[0u8; 6]);
    let err = read_xls(&xls(&workbook(&g, &[("S", 0, 0, sheet(&[]))]))).unwrap_err().to_string();
    assert!(err.contains("password-protected"), "{err}");

    // An Excel 5/95 file keeps its records in a `Book` stream.
    let book = cfb(&[("Book", &[0u8; 64])]);
    assert_eq!(sniff(&book), Format::Xls);
    assert!(read_xls(&book).unwrap_err().to_string().contains("Excel 5.0/95"));
    // A BIFF5 BOF inside a Workbook stream.
    let mut old = rec(0x0809, &[0x00, 0x05, 0x05, 0x00]);
    old.extend(eof());
    assert!(read_xls(&xls(&old)).unwrap_err().to_string().contains("Excel 5.0/95"));

    // A compound file without a workbook stream is another legacy binary.
    let doc = cfb(&[("WordDocument", &[0u8; 100])]);
    assert_eq!(sniff(&doc), Format::LegacyBinary);
    assert!(read_xls(&doc).is_err());
    assert_eq!(sniff(&crate::cfb::tests::build(&["EncryptionInfo", "EncryptedPackage"])), Format::Encrypted);
}

#[test]
fn xls_sheet_offsets_and_records_are_validated() {
    // A sheet pointing into the middle of a record.
    let mut stream = workbook(&[], &[("S", 0, 0, sheet(&number(0, 0, 15, 1.0)))]);
    let pos = stream.windows(2).position(|w| w == [0x85, 0x00]).unwrap() + 4;
    stream[pos] = stream[pos].wrapping_add(1);
    assert!(read_xls(&xls(&stream)).is_err());
    // A workbook with only a chart sheet has no worksheets.
    assert!(read_xls(&xls(&workbook(&[], &[("C", 0, 2, [bof(0x20), eof()].concat())]))).is_err());
    // Out-of-grid MULRK columns are dropped, not wrapped.
    let mut m = [0u16.to_le_bytes(), 16_383u16.to_le_bytes()].concat();
    for n in 0..3 {
        m.extend(15u16.to_le_bytes());
        m.extend(rk_int(n).to_le_bytes());
    }
    m.extend(16_385u16.to_le_bytes());
    let (wb, _) = read_xls(&simple(&rec(0x00BD, &m))).unwrap();
    assert_eq!(wb.sheets[0].cells.len(), 1);
}

#[test]
fn xls_hostile_bytes_never_panic() {
    let base = rich_fixture();
    let small = simple(
        &[number(0, 0, 15, 1.0), formula(0, 1, 15, num(2.0), &[ptg_ref(0x24, 0, 0, true, true), ptg_int(2), vec![0x05]].concat(), &[])].concat(),
    );
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for input in [&base, &small] {
        for cut in (0..input.len()).step_by(97) {
            let _ = read_xls(&input[..cut]);
            let _ = sniff(&input[..cut]);
        }
        for _ in 0..1500 {
            let mut b = input.clone();
            for _ in 0..1 + next() % 8 {
                let i = (next() as usize) % b.len();
                b[i] = next() as u8;
            }
            let _ = read_xls(&b);
        }
    }
    // Hostile formula streams: every token type with truncated and random operands.
    for t in 0u8..=0x7F {
        for len in 0..12 {
            let mut rgce = vec![t];
            rgce.extend((0..len).map(|_| next() as u8));
            let _ = read_xls(&simple(&formula(0, 0, 15, num(0.0), &rgce, &[0xFF, 0xFF, 0xFF])));
        }
    }
    // Hostile SST counts and string lengths.
    let mut sst = u32::MAX.to_le_bytes().to_vec();
    sst.extend(3u32.to_le_bytes());
    sst.extend([0xFF, 0xFF, 0x09, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]);
    let _ = read_xls(&xls(&workbook(&rec(0x00FC, &sst), &[("S", 0, 0, sheet(&[]))])));
    let mut sst = 0u32.to_le_bytes().to_vec();
    sst.extend(u32::MAX.to_le_bytes());
    assert!(read_xls(&xls(&workbook(&rec(0x00FC, &sst), &[("S", 0, 0, sheet(&[]))]))).is_err());
}
