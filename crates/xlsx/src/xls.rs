//! Excel 97-2003 workbook (`.xls`, BIFF8) import, implemented from Microsoft's public [MS-XLS]
//! specification: <https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/cd03cb5f-ca02-4934-a391-bb674cb8aa06>.
//!
//! Imported: worksheet values and saved formula results, formulas (decoded from their parsed
//! tokens back to formula text; shared and legacy array formulas included), number formats,
//! fonts, fills, borders, alignment, merged cells, column widths, row heights, hidden rows,
//! columns and sheets, frozen panes, sheet view options, defined names and the 1904 date system.
//! Charts, drawings, comments, hyperlinks, conditional formats, validation, macros and
//! password-protected files are not imported; the [`ReadReport`] says what was skipped.
//!
//! Every read is bounds-checked, every loop bounded and every input-sized allocation capped.

use std::collections::HashMap;
use std::sync::Arc;

use gridcraft_core::{CellError, CellRef, DateSystem, MAX_COLS, MAX_ROWS, RangeRef, Value, col_to_letters};
use gridcraft_formula::printer::needs_quotes;
use gridcraft_model::{
    Alignment, BorderLine, BorderStyle, Borders, Cell, Color, DefinedName, Fill, Font, Formula, HAlign, LineInfo, NumFmt, PatternType, Sheet, Style,
    StyleId, Underline, VAlign, VertAlign, Visibility, Workbook,
};

use gridcraft_model::style::Protection;

use crate::cfb::Compound;
use crate::tables::{INDEXED, builtin_format, col_width_to_px, pt_to_px};
use crate::{IoError, ReadReport};

const MAX_RECORDS: usize = 10_000_000;
const MAX_CELLS: usize = 2_000_000;
const MAX_TEXT: usize = 64 * 1024 * 1024;
const MAX_STRINGS: usize = 2_000_000;
const MAX_SHEETS: usize = 1024;
const MAX_XFS: usize = 65_536;
const MAX_MERGES: usize = 100_000;
const MAX_NAMES: usize = 65_536;
/// Formula evaluation stack depth and decoded formula length, both well above Excel's limits.
const MAX_STACK: usize = 1024;
const MAX_FORMULA: usize = 16_384;
/// Array constant cells.
const MAX_ARRAY: usize = 65_536;

/// BIFF8 record types used here ([MS-XLS] 2.3).
mod rt {
    pub const FORMULA: u16 = 0x0006;
    pub const EOF: u16 = 0x000A;
    pub const EXTERNSHEET: u16 = 0x0017;
    pub const NAME: u16 = 0x0018;
    pub const NOTE: u16 = 0x001C;
    pub const DATEMODE: u16 = 0x0022;
    pub const EXTERNNAME: u16 = 0x0023;
    pub const FILEPASS: u16 = 0x002F;
    pub const FONT: u16 = 0x0031;
    pub const CONTINUE: u16 = 0x003C;
    pub const WINDOW1: u16 = 0x003D;
    pub const PANE: u16 = 0x0041;
    pub const DEFCOLWIDTH: u16 = 0x0055;
    pub const OBJ: u16 = 0x005D;
    pub const COLINFO: u16 = 0x007D;
    pub const BOUNDSHEET: u16 = 0x0085;
    pub const PALETTE: u16 = 0x0092;
    pub const AUTOFILTERINFO: u16 = 0x009D;
    pub const SCL: u16 = 0x00A0;
    pub const MULRK: u16 = 0x00BD;
    pub const MULBLANK: u16 = 0x00BE;
    pub const XF: u16 = 0x00E0;
    pub const MERGECELLS: u16 = 0x00E5;
    pub const MSODRAWING: u16 = 0x00EC;
    pub const SST: u16 = 0x00FC;
    pub const LABELSST: u16 = 0x00FD;
    pub const SUPBOOK: u16 = 0x01AE;
    pub const CONDFMT: u16 = 0x01B0;
    pub const DVAL: u16 = 0x01B2;
    pub const HLINK: u16 = 0x01B8;
    pub const BLANK: u16 = 0x0201;
    pub const NUMBER: u16 = 0x0203;
    pub const LABEL: u16 = 0x0204;
    pub const BOOLERR: u16 = 0x0205;
    pub const STRING: u16 = 0x0207;
    pub const ROW: u16 = 0x0208;
    pub const ARRAY: u16 = 0x0221;
    pub const DEFAULTROWHEIGHT: u16 = 0x0225;
    pub const TABLE: u16 = 0x0236;
    pub const WINDOW2: u16 = 0x023E;
    pub const RK: u16 = 0x027E;
    pub const FORMAT: u16 = 0x041E;
    pub const SHRFMLA: u16 = 0x04BC;
    pub const BOF: u16 = 0x0809;
}

fn invalid(msg: impl Into<String>) -> IoError {
    IoError::Format(format!("XLS: {}", msg.into()))
}

#[derive(Default)]
struct Budget {
    cells: usize,
    text: usize,
}
impl Budget {
    fn text(&mut self, len: usize) -> Result<(), IoError> {
        self.text = self.text.saturating_add(len);
        if self.text > MAX_TEXT {
            return Err(IoError::TooLarge("XLS decoded text exceeds 64 MiB".into()));
        }
        Ok(())
    }
    fn cell(&mut self) -> Result<(), IoError> {
        self.cells += 1;
        if self.cells > MAX_CELLS {
            return Err(IoError::TooLarge("XLS exceeds 2 million imported cells".into()));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ records and byte reading

/// One record: its body plus the bodies of the CONTINUE records that follow it.
struct Record<'a> {
    offset: usize,
    kind: u16,
    data: &'a [u8],
    cont: Vec<&'a [u8]>,
}

impl<'a> Record<'a> {
    fn cur(&self) -> Cur<'a> {
        let mut parts = Vec::with_capacity(1 + self.cont.len());
        parts.push(self.data);
        parts.extend(self.cont.iter().copied());
        Cur { parts, part: 0, pos: 0 }
    }
}

/// Splits a BIFF8 stream into records. Trailing bytes too short for a record (stream padding)
/// end the list.
fn records(stream: &[u8]) -> Result<Vec<Record<'_>>, IoError> {
    let mut out: Vec<Record<'_>> = Vec::new();
    let mut pos = 0usize;
    while let (Some(kind), Some(len)) = (u16_at(stream, pos), u16_at(stream, pos + 2)) {
        let start = pos + 4;
        let Some(data) = stream.get(start..start + usize::from(len)) else { break };
        if out.len() >= MAX_RECORDS {
            return Err(IoError::TooLarge("XLS stream contains too many records".into()));
        }
        if kind == rt::CONTINUE
            && let Some(last) = out.last_mut()
        {
            last.cont.push(data);
        } else {
            out.push(Record { offset: pos, kind, data, cont: vec![] });
        }
        pos = start + usize::from(len);
    }
    Ok(out)
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*s.first()?, *s.get(1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([*s.first()?, *s.get(1)?, *s.get(2)?, *s.get(3)?]))
}

fn f64_at(b: &[u8], at: usize) -> Option<f64> {
    let s: [u8; 8] = b.get(at..at.checked_add(8)?)?.try_into().ok()?;
    Some(f64::from_le_bytes(s))
}

/// A cursor over a record and its CONTINUE records. Raw reads run across record boundaries;
/// character reads re-read the encoding flag at each boundary, as [MS-XLS] 2.5.293 requires.
struct Cur<'a> {
    parts: Vec<&'a [u8]>,
    part: usize,
    pos: usize,
}

impl<'a> Cur<'a> {
    fn slice(b: &'a [u8]) -> Cur<'a> {
        Cur { parts: vec![b], part: 0, pos: 0 }
    }
    fn current(&self) -> &'a [u8] {
        self.parts.get(self.part).copied().unwrap_or(&[])
    }
    fn remaining(&self) -> usize {
        let rest: usize = self.parts.iter().skip(self.part + 1).map(|p| p.len()).sum();
        self.current().len().saturating_sub(self.pos) + rest
    }
    fn u8(&mut self) -> Option<u8> {
        while self.pos >= self.current().len() {
            if self.part + 1 >= self.parts.len() {
                return None;
            }
            self.part += 1;
            self.pos = 0;
        }
        let b = *self.current().get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes([self.u8()?, self.u8()?]))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes([self.u8()?, self.u8()?, self.u8()?, self.u8()?]))
    }
    fn f64(&mut self) -> Option<f64> {
        let mut b = [0u8; 8];
        for x in &mut b {
            *x = self.u8()?;
        }
        Some(f64::from_le_bytes(b))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        if n > self.remaining() {
            return None;
        }
        let mut left = n;
        while left > 0 {
            let avail = self.current().len().saturating_sub(self.pos);
            if avail == 0 {
                self.part += 1;
                self.pos = 0;
                continue;
            }
            let take = avail.min(left);
            self.pos += take;
            left -= take;
        }
        Some(())
    }
    /// `cch` characters, 8-bit (`high == false`) or UTF-16LE. A string split across records
    /// continues with a fresh flag byte.
    fn chars(&mut self, cch: usize, mut high: bool) -> Option<String> {
        let mut units: Vec<u16> = Vec::with_capacity(cch.min(self.remaining()));
        while units.len() < cch {
            if self.pos >= self.current().len() {
                if self.part + 1 >= self.parts.len() {
                    return None;
                }
                self.part += 1;
                self.pos = 0;
                high = self.current().first()? & 1 != 0;
                self.pos = 1;
                continue;
            }
            let cur = self.current();
            let avail = cur.len() - self.pos;
            let need = cch - units.len();
            if high {
                let n = (avail / 2).min(need);
                if n == 0 {
                    return None;
                }
                let s = cur.get(self.pos..self.pos + n * 2)?;
                units.extend(s.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)));
                self.pos += n * 2;
            } else {
                let n = avail.min(need);
                units.extend(cur.get(self.pos..self.pos + n)?.iter().map(|&b| u16::from(b)));
                self.pos += n;
            }
        }
        Some(String::from_utf16_lossy(&units))
    }
    /// XLUnicodeString: 16-bit count, flags, characters.
    fn xl_string(&mut self) -> Option<String> {
        let cch = usize::from(self.u16()?);
        let flags = self.u8()?;
        self.chars(cch, flags & 1 != 0)
    }
    /// ShortXLUnicodeString: 8-bit count, flags, characters.
    fn short_string(&mut self) -> Option<String> {
        let cch = usize::from(self.u8()?);
        let flags = self.u8()?;
        self.chars(cch, flags & 1 != 0)
    }
    /// XLUnicodeRichExtendedString (shared strings): runs and phonetic data are skipped.
    fn rich_string(&mut self) -> Option<String> {
        let cch = usize::from(self.u16()?);
        let flags = self.u8()?;
        let runs = if flags & 0x08 != 0 { usize::from(self.u16()?) } else { 0 };
        let ext = if flags & 0x04 != 0 { self.u32()? as usize } else { 0 };
        let s = self.chars(cch, flags & 1 != 0)?;
        self.skip(runs.checked_mul(4)?)?;
        self.skip(ext)?;
        Some(s)
    }
}

// ------------------------------------------------------------------ globals

#[derive(Clone, Copy, PartialEq)]
enum SupKind {
    Internal,
    AddIn,
    External,
}

struct SupBook {
    kind: SupKind,
    names: Vec<String>,
}

struct SheetInfo {
    name: String,
    offset: usize,
    visibility: Visibility,
    /// 0 worksheet or dialog, 1 macro sheet, 2 chart, 6 VB module.
    kind: u8,
}

struct XfRec {
    font: u16,
    fmt: u16,
    locked: bool,
    hidden: bool,
    align: Alignment,
    borders: [(u8, u16); 6],
    pattern: u8,
    fore: u16,
    back: u16,
}

struct RawName {
    text: String,
    scope: u16,
    hidden: bool,
    /// A function or command name, or a built-in name: never imported as a defined name.
    special: bool,
    builtin: Option<u8>,
    rgce: Vec<u8>,
    rgcb: Vec<u8>,
}

/// What formulas need to turn tokens back into text.
struct Names {
    sheets: Vec<String>,
    xti: Vec<(u16, i16, i16)>,
    supbooks: Vec<SupBook>,
    names: Vec<String>,
}

#[derive(Default)]
struct Globals {
    date_1904: bool,
    sheets: Vec<SheetInfo>,
    sst: Vec<Arc<str>>,
    formats: HashMap<u16, String>,
    fonts: Vec<Font>,
    font_colors: Vec<u16>,
    xfs: Vec<XfRec>,
    palette: Vec<u32>,
    names: Vec<RawName>,
    xti: Vec<(u16, i16, i16)>,
    supbooks: Vec<SupBook>,
    active_tab: usize,
}

/// Imports an Excel 97-2003 (BIFF8) workbook. Inspect the report's warnings.
pub fn read_xls(bytes: &[u8]) -> Result<(Workbook, ReadReport), IoError> {
    let cfb = Compound::open(bytes).map_err(invalid)?;
    if cfb.stream("Workbook").map_err(invalid)?.is_none() && cfb.has_stream("Book") {
        return Err(invalid("this is an Excel 5.0/95 workbook (BIFF5). GridCraft opens Excel 97-2003 and newer files; resave it in a newer Excel"));
    }
    let stream = cfb.stream("Workbook").map_err(invalid)?.ok_or_else(|| invalid("no Workbook stream: this is not an Excel workbook"))?;
    let recs = records(&stream)?;
    let mut report = ReadReport::default();
    let mut budget = Budget::default();
    let g = globals(&recs, &mut budget, &mut report)?;

    let mut wb = Workbook::new();
    wb.sheets.clear();
    wb.date_system = if g.date_1904 { DateSystem::D1904 } else { DateSystem::D1900 };
    let names = Names {
        sheets: g.sheets.iter().map(|s| s.name.clone()).collect(),
        xti: g.xti.clone(),
        supbooks: g.supbooks.iter().map(|s| SupBook { kind: s.kind, names: s.names.clone() }).collect(),
        names: g.names.iter().map(|n| n.text.clone()).collect(),
    };
    let mut styles = StyleMap { cache: vec![None; g.xfs.len()] };
    let mut formulas = FormulaStats::default();
    // BoundSheet index → imported sheet index.
    let mut sheet_map: Vec<Option<usize>> = Vec::with_capacity(g.sheets.len());
    for info in &g.sheets {
        if info.kind != 0 {
            report.warn(match info.kind {
                2 => "Chart sheets in .xls files are not imported.",
                1 => "Excel 4.0 macro sheets in .xls files are not imported.",
                _ => "Macro modules and dialog sheets in .xls files are not imported.",
            });
            sheet_map.push(None);
            continue;
        }
        let start = recs
            .binary_search_by_key(&info.offset, |r| r.offset)
            .map_err(|_| invalid(format!("sheet '{}' points outside the workbook stream", info.name)))?;
        let mut sheet = read_sheet(&recs, start, &info.name, &g, &names, &mut wb, &mut styles, &mut budget, &mut formulas, &mut report)?;
        sheet.visibility = info.visibility;
        if wb.check_sheet_name(&sheet.name, None).is_err() {
            let fresh = wb.next_sheet_name();
            report.warn(format!("Sheet '{}' had a name GridCraft can't use; it was renamed '{fresh}'.", sheet.name));
            sheet.name = fresh;
        }
        sheet_map.push(Some(wb.sheets.len()));
        wb.sheets.push(Arc::new(sheet));
    }
    if wb.sheets.is_empty() {
        return Err(invalid("no worksheets"));
    }
    defined_names(&g, &names, &sheet_map, &mut wb, &mut formulas, &mut report);
    formulas.report(&mut report);
    let active = sheet_map.get(g.active_tab).copied().flatten().unwrap_or(0);
    wb.active_sheet = if wb.sheets.get(active).is_some_and(|s| s.visibility == Visibility::Visible) {
        active
    } else {
        wb.sheets.iter().position(|s| s.visibility == Visibility::Visible).unwrap_or(0)
    };
    Ok((wb, report))
}

fn globals(recs: &[Record<'_>], budget: &mut Budget, report: &mut ReadReport) -> Result<Globals, IoError> {
    let mut g = Globals { palette: INDEXED.to_vec(), ..Default::default() };
    let first = recs.first().ok_or_else(|| invalid("empty workbook stream"))?;
    if first.kind != rt::BOF {
        return Err(invalid("the workbook stream doesn't start with a BOF record"));
    }
    let vers = u16_at(first.data, 0).unwrap_or(0);
    let dt = u16_at(first.data, 2).unwrap_or(0);
    if vers != 0x0600 {
        return Err(invalid("this is an Excel 5.0/95 or older workbook. GridCraft opens Excel 97-2003 and newer files; resave it in a newer Excel"));
    }
    if dt != 0x0005 {
        return Err(invalid("the workbook stream doesn't start with workbook globals"));
    }
    for r in recs.iter().skip(1) {
        match r.kind {
            rt::EOF => break,
            rt::FILEPASS => {
                return Err(invalid(
                    "this .xls workbook is password-protected. GridCraft can't open encrypted workbooks yet; remove the password in Excel and try again",
                ));
            }
            rt::DATEMODE => g.date_1904 = u16_at(r.data, 0) == Some(1),
            rt::WINDOW1 => g.active_tab = usize::from(u16_at(r.data, 10).unwrap_or(0)),
            rt::BOUNDSHEET => {
                if g.sheets.len() >= MAX_SHEETS {
                    return Err(IoError::TooLarge("XLS exceeds 1024 sheets".into()));
                }
                let mut c = r.cur();
                let offset = c.u32().ok_or_else(|| invalid("truncated sheet record"))? as usize;
                let state = c.u8().ok_or_else(|| invalid("truncated sheet record"))?;
                let kind = c.u8().ok_or_else(|| invalid("truncated sheet record"))?;
                let name = c.short_string().ok_or_else(|| invalid("truncated sheet name"))?;
                budget.text(name.len())?;
                let visibility = match state & 3 {
                    0 => Visibility::Visible,
                    1 => Visibility::Hidden,
                    _ => Visibility::VeryHidden,
                };
                g.sheets.push(SheetInfo { name, offset, visibility, kind });
            }
            rt::SST => g.sst = shared_strings(r, budget, report)?,
            rt::FORMAT => {
                let mut c = r.cur();
                if let (Some(id), Some(code)) = (c.u16(), c.xl_string()) {
                    budget.text(code.len())?;
                    g.formats.insert(id, code);
                }
            }
            rt::FONT => {
                if g.fonts.len() < MAX_XFS {
                    let (font, icv) = font(r);
                    g.fonts.push(font);
                    g.font_colors.push(icv);
                }
            }
            rt::XF => {
                if g.xfs.len() >= MAX_XFS {
                    return Err(IoError::TooLarge("XLS has too many cell formats".into()));
                }
                g.xfs.push(xf(r.data));
            }
            rt::PALETTE => {
                let n = usize::from(u16_at(r.data, 0).unwrap_or(0)).min(56);
                for i in 0..n {
                    let at = 2 + i * 4;
                    if let Some(c) = r.data.get(at..at + 3)
                        && let Some(slot) = g.palette.get_mut(8 + i)
                    {
                        *slot = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
                    }
                }
            }
            rt::SUPBOOK => {
                let cch = u16_at(r.data, 2).unwrap_or(0);
                let kind = match cch {
                    0x0401 => SupKind::Internal,
                    0x3A01 => SupKind::AddIn,
                    _ => SupKind::External,
                };
                g.supbooks.push(SupBook { kind, names: vec![] });
            }
            rt::EXTERNNAME => {
                let mut c = r.cur();
                let name = c.skip(6).and_then(|_| c.short_string()).unwrap_or_default();
                if let Some(s) = g.supbooks.last_mut()
                    && s.names.len() < MAX_NAMES
                {
                    budget.text(name.len())?;
                    s.names.push(name);
                }
            }
            rt::EXTERNSHEET => {
                let mut c = r.cur();
                let n = usize::from(c.u16().unwrap_or(0));
                for _ in 0..n {
                    let (Some(s), Some(a), Some(b)) = (c.u16(), c.u16(), c.u16()) else { break };
                    g.xti.push((s, a as i16, b as i16));
                }
            }
            rt::NAME => {
                if g.names.len() >= MAX_NAMES {
                    return Err(IoError::TooLarge("XLS has too many defined names".into()));
                }
                g.names.push(raw_name(r, budget)?);
            }
            _ => {}
        }
    }
    Ok(g)
}

fn shared_strings(r: &Record<'_>, budget: &mut Budget, report: &mut ReadReport) -> Result<Vec<Arc<str>>, IoError> {
    let mut c = r.cur();
    let _total = c.u32();
    let unique = c.u32().unwrap_or(0) as usize;
    if unique > MAX_STRINGS {
        return Err(IoError::TooLarge("XLS has too many shared strings".into()));
    }
    let mut out = Vec::with_capacity(unique.min(c.remaining() / 3 + 1));
    for _ in 0..unique {
        let Some(s) = c.rich_string() else {
            report.warn("The .xls shared-string table is shorter than it says; cells using missing strings were left blank.");
            break;
        };
        budget.text(s.len())?;
        out.push(Arc::from(s));
    }
    Ok(out)
}

fn font(r: &Record<'_>) -> (Font, u16) {
    let d = r.data;
    let mut f = Font::default();
    if let Some(h) = u16_at(d, 0)
        && h > 0
    {
        f.size = (f32::from(h) / 20.0).clamp(1.0, 409.0);
    }
    let grbit = u16_at(d, 2).unwrap_or(0);
    f.italic = grbit & 0x02 != 0;
    f.strike = grbit & 0x08 != 0;
    let icv = u16_at(d, 4).unwrap_or(0x7FFF);
    f.bold = u16_at(d, 6).unwrap_or(400) >= 700;
    f.vert = match u16_at(d, 8).unwrap_or(0) {
        1 => VertAlign::Superscript,
        2 => VertAlign::Subscript,
        _ => VertAlign::Baseline,
    };
    f.underline = match d.get(10).copied().unwrap_or(0) {
        0x01 => Underline::Single,
        0x02 => Underline::Double,
        0x21 => Underline::SingleAccounting,
        0x22 => Underline::DoubleAccounting,
        _ => Underline::None,
    };
    let mut c = Cur { parts: std::iter::once(d).chain(r.cont.iter().copied()).collect(), part: 0, pos: 0 };
    if c.skip(14).is_some()
        && let Some(name) = c.short_string()
        && !name.trim().is_empty()
    {
        f.name = name;
    }
    (f, icv)
}

fn xf(d: &[u8]) -> XfRec {
    let flags = u16_at(d, 4).unwrap_or(0);
    let a = d.get(6).copied().unwrap_or(0);
    let trot = d.get(7).copied().unwrap_or(0);
    let ind = d.get(8).copied().unwrap_or(0);
    let b1 = u32_at(d, 10).unwrap_or(0);
    let b2 = u32_at(d, 14).unwrap_or(0);
    let fill = u16_at(d, 18).unwrap_or(0);
    let align = Alignment {
        h: match a & 7 {
            1 => HAlign::Left,
            2 => HAlign::Center,
            3 => HAlign::Right,
            4 => HAlign::Fill,
            5 => HAlign::Justify,
            6 => HAlign::CenterAcross,
            7 => HAlign::Distributed,
            _ => HAlign::General,
        },
        v: match (a >> 4) & 7 {
            0 => VAlign::Top,
            1 => VAlign::Center,
            3 => VAlign::Justify,
            4 => VAlign::Distributed,
            _ => VAlign::Bottom,
        },
        wrap: a & 0x08 != 0,
        shrink: ind & 0x10 != 0,
        indent: ind & 0x0F,
        rotation: if trot <= 180 || trot == 255 { i16::from(trot) } else { 0 },
    };
    let diag = ((b1 >> 30) & 3) as u8;
    let diag_style = ((b2 >> 21) & 0x0F) as u8;
    let diag_color = ((b2 >> 14) & 0x7F) as u16;
    XfRec {
        font: u16_at(d, 0).unwrap_or(0),
        fmt: u16_at(d, 2).unwrap_or(0),
        locked: flags & 1 != 0,
        hidden: flags & 2 != 0,
        align,
        borders: [
            ((b1 & 0x0F) as u8, ((b1 >> 16) & 0x7F) as u16),
            (((b1 >> 4) & 0x0F) as u8, ((b1 >> 23) & 0x7F) as u16),
            (((b1 >> 8) & 0x0F) as u8, (b2 & 0x7F) as u16),
            (((b1 >> 12) & 0x0F) as u8, ((b2 >> 7) & 0x7F) as u16),
            (if diag & 1 != 0 { diag_style } else { 0 }, diag_color),
            (if diag & 2 != 0 { diag_style } else { 0 }, diag_color),
        ],
        pattern: ((b2 >> 26) & 0x3F) as u8,
        fore: fill & 0x7F,
        back: (fill >> 7) & 0x7F,
    }
}

fn raw_name(r: &Record<'_>, budget: &mut Budget) -> Result<RawName, IoError> {
    let bad = || invalid("truncated defined name");
    let mut c = r.cur();
    let grbit = c.u16().ok_or_else(bad)?;
    let _key = c.u8().ok_or_else(bad)?;
    let cch = usize::from(c.u8().ok_or_else(bad)?);
    let cce = usize::from(c.u16().ok_or_else(bad)?);
    c.skip(2).ok_or_else(bad)?;
    let scope = c.u16().ok_or_else(bad)?;
    c.skip(4).ok_or_else(bad)?;
    let flags = c.u8().ok_or_else(bad)?;
    let builtin = grbit & 0x20 != 0;
    let (text, code) = if builtin {
        let code = if flags & 1 != 0 { c.u16().map(|v| v as u8) } else { c.u8() }.ok_or_else(bad)?;
        c.skip(cch.saturating_sub(1) * if flags & 1 != 0 { 2 } else { 1 }).ok_or_else(bad)?;
        (builtin_name(code).to_string(), Some(code))
    } else {
        (c.chars(cch, flags & 1 != 0).ok_or_else(bad)?, None)
    };
    budget.text(text.len())?;
    let mut rgce = Vec::with_capacity(cce.min(c.remaining()));
    for _ in 0..cce {
        rgce.push(c.u8().ok_or_else(bad)?);
    }
    let mut rgcb = Vec::with_capacity(c.remaining());
    while let Some(b) = c.u8() {
        rgcb.push(b);
    }
    Ok(RawName { text, scope, hidden: grbit & 1 != 0, special: grbit & 0x0E != 0 || builtin, builtin: code, rgce, rgcb })
}

/// Built-in name codes ([MS-XLS] 2.5.149 Lbl).
fn builtin_name(code: u8) -> &'static str {
    match code {
        0x00 => "Consolidate_Area",
        0x01 => "Auto_Open",
        0x02 => "Auto_Close",
        0x03 => "Extract",
        0x04 => "Database",
        0x05 => "Criteria",
        0x06 => "Print_Area",
        0x07 => "Print_Titles",
        0x08 => "Recorder",
        0x09 => "Data_Form",
        0x0A => "Auto_Activate",
        0x0B => "Auto_Deactivate",
        0x0C => "Sheet_Title",
        0x0D => "_FilterDatabase",
        _ => "_Builtin",
    }
}

fn defined_names(g: &Globals, names: &Names, sheet_map: &[Option<usize>], wb: &mut Workbook, formulas: &mut FormulaStats, report: &mut ReadReport) {
    for n in &g.names {
        if n.special {
            if matches!(n.builtin, Some(0x06 | 0x07)) {
                report.warn("Print areas and print titles in .xls files are not imported.");
            }
            continue;
        }
        let scope = match n.scope {
            0 => None,
            i => match sheet_map.get(usize::from(i) - 1).copied().flatten() {
                Some(s) => Some(s),
                None => continue,
            },
        };
        match decode(&n.rgce, &n.rgcb, CellRef::default(), names) {
            Ok(text) if !text.is_empty() && gridcraft_formula::parse(&text).is_ok() => {
                wb.names.push(DefinedName { name: n.text.clone(), scope, formula: text, comment: String::new(), hidden: n.hidden });
            }
            Ok(_) | Err(_) => formulas.dropped_names += 1,
        }
    }
}

// ------------------------------------------------------------------ styles

struct StyleMap {
    cache: Vec<Option<StyleId>>,
}

/// The default cell format: XF 15 in files Excel writes ([MS-XLS] 2.4.353).
const DEFAULT_XF: u16 = 15;

impl StyleMap {
    fn get(&mut self, ixfe: u16, g: &Globals, wb: &mut Workbook) -> StyleId {
        let i = usize::from(ixfe);
        if let Some(Some(id)) = self.cache.get(i) {
            return *id;
        }
        let Some(x) = g.xfs.get(i) else { return StyleId::DEFAULT };
        let color = |icv: u16| -> Color {
            match g.palette.get(usize::from(icv)) {
                Some(&rgb) if icv < 64 => Color::Rgb(rgb),
                _ => Color::Auto,
            }
        };
        let fi = if x.font >= 4 { usize::from(x.font) - 1 } else { usize::from(x.font) };
        let mut font = g.fonts.get(fi).cloned().unwrap_or_default();
        font.color = g.font_colors.get(fi).map_or(Color::Auto, |&icv| color(icv));
        let line = |(dg, icv): (u8, u16)| BorderLine {
            style: match dg {
                1 => BorderStyle::Thin,
                2 => BorderStyle::Medium,
                3 => BorderStyle::Dashed,
                4 => BorderStyle::Dotted,
                5 => BorderStyle::Thick,
                6 => BorderStyle::Double,
                7 => BorderStyle::Hair,
                8 => BorderStyle::MediumDashed,
                9 => BorderStyle::DashDot,
                10 => BorderStyle::MediumDashDot,
                11 => BorderStyle::DashDotDot,
                12 => BorderStyle::MediumDashDotDot,
                13 => BorderStyle::SlantDashDot,
                _ => BorderStyle::None,
            },
            color: if dg == 0 { Color::Auto } else { color(icv) },
        };
        let [l, r, t, b, dd, du] = x.borders;
        let pattern = match x.pattern {
            1 => PatternType::Solid,
            2 => PatternType::Gray50,
            3 => PatternType::Gray75,
            4 => PatternType::Gray25,
            5 => PatternType::DarkHorizontal,
            6 => PatternType::DarkVertical,
            7 => PatternType::DarkDown,
            8 => PatternType::DarkUp,
            9 => PatternType::DarkGrid,
            10 => PatternType::DarkTrellis,
            11 => PatternType::LightHorizontal,
            12 => PatternType::LightVertical,
            13 => PatternType::LightDown,
            14 => PatternType::LightUp,
            15 => PatternType::LightGrid,
            16 => PatternType::LightTrellis,
            17 => PatternType::Gray125,
            18 => PatternType::Gray0625,
            _ => PatternType::None,
        };
        let fill = if pattern == PatternType::None { Fill::default() } else { Fill { pattern, fg: color(x.fore), bg: color(x.back) } };
        let code = g.formats.get(&x.fmt).map(String::as_str).or_else(|| builtin_format(u32::from(x.fmt))).unwrap_or("General");
        let style = Style {
            font,
            fill,
            borders: Borders { left: line(l), right: line(r), top: line(t), bottom: line(b), diag_down: line(dd), diag_up: line(du) },
            align: x.align,
            num_fmt: NumFmt::new(code),
            protection: Protection { locked: x.locked, hidden: x.hidden },
        };
        let id = wb.styles.intern(style);
        if let Some(slot) = self.cache.get_mut(i) {
            *slot = Some(id);
        }
        id
    }
}

// ------------------------------------------------------------------ worksheets

#[derive(Default)]
struct FormulaStats {
    dropped: usize,
    dropped_names: usize,
    tables: usize,
    reasons: Vec<String>,
}

impl FormulaStats {
    fn drop(&mut self, why: String) {
        self.dropped += 1;
        if self.reasons.len() < 3 && !self.reasons.contains(&why) {
            self.reasons.push(why);
        }
    }
    fn report(&self, report: &mut ReadReport) {
        if self.dropped > 0 {
            report.warn(format!(
                "{} formula{} in this .xls file use features GridCraft can't read ({}); their saved results were kept as values.",
                self.dropped,
                if self.dropped == 1 { "" } else { "s" },
                self.reasons.join("; ")
            ));
        }
        if self.dropped_names > 0 {
            report.warn(format!("{} defined name{} could not be imported.", self.dropped_names, if self.dropped_names == 1 { "" } else { "s" }));
        }
        if self.tables > 0 {
            report.warn("What-if data tables in .xls files are not supported; their saved values were kept.");
        }
    }
}

/// A shared or array formula body, keyed by its anchor cell.
struct Body {
    range: RangeRef,
    rgce: Vec<u8>,
    rgcb: Vec<u8>,
}

fn cell_at(d: &[u8]) -> Option<(CellRef, u16)> {
    let row = u32::from(u16_at(d, 0)?);
    let col = u32::from(u16_at(d, 2)?);
    if row >= MAX_ROWS || col >= MAX_COLS {
        return None;
    }
    Some((CellRef::new(row, col), u16_at(d, 4)?))
}

/// An RK number ([MS-XLS] 2.5.217): a 30-bit integer or the top 30 bits of a double, maybe ×100.
fn rk(v: u32) -> f64 {
    let n = if v & 2 != 0 { f64::from((v as i32) >> 2) } else { f64::from_bits(u64::from(v & 0xFFFF_FFFC) << 32) };
    if v & 1 != 0 { n / 100.0 } else { n }
}

fn error_value(code: u8) -> CellError {
    match code {
        0x00 => CellError::Null,
        0x07 => CellError::Div0,
        0x0F => CellError::Value,
        0x17 => CellError::Ref,
        0x1D => CellError::Name,
        0x24 => CellError::Num,
        0x2B => CellError::GettingData,
        _ => CellError::NA,
    }
}

fn error_text(code: u8) -> &'static str {
    match code {
        0x00 => "#NULL!",
        0x07 => "#DIV/0!",
        0x0F => "#VALUE!",
        0x17 => "#REF!",
        0x1D => "#NAME?",
        0x24 => "#NUM!",
        0x2B => "#GETTING_DATA",
        _ => "#N/A",
    }
}

#[allow(clippy::too_many_arguments)]
fn read_sheet(
    recs: &[Record<'_>],
    start: usize,
    name: &str,
    g: &Globals,
    names: &Names,
    wb: &mut Workbook,
    styles: &mut StyleMap,
    budget: &mut Budget,
    formulas: &mut FormulaStats,
    report: &mut ReadReport,
) -> Result<Sheet, IoError> {
    let mut sheet = Sheet::new(name);
    let first = recs.get(start).ok_or_else(|| invalid("missing sheet"))?;
    if first.kind != rt::BOF {
        return Err(invalid(format!("sheet '{name}' doesn't start with a BOF record")));
    }
    let mut depth = 0usize;
    let mut frozen = false;
    let mut pending_string: Option<CellRef> = None;
    let mut shared: HashMap<CellRef, Body> = HashMap::new();
    let mut arrays: HashMap<CellRef, Body> = HashMap::new();
    let mut deferred: Vec<(CellRef, CellRef)> = Vec::new();
    let set = |sheet: &mut Sheet,
               wb: &mut Workbook,
               styles: &mut StyleMap,
               budget: &mut Budget,
               pos: CellRef,
               ixfe: u16,
               value: Value|
     -> Result<(), IoError> {
        budget.cell()?;
        if let Value::Text(t) = &value {
            budget.text(t.len())?;
        }
        let style = styles.get(ixfe, g, wb);
        sheet.cells.set(pos, Cell { value, formula: None, style });
        Ok(())
    };
    for r in recs.iter().skip(start) {
        match r.kind {
            rt::BOF => {
                depth += 1;
                if depth > 1 {
                    // An embedded chart's substream.
                    report.warn("Charts, pictures and drawings in .xls files are not imported.");
                }
                continue;
            }
            rt::EOF => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
                continue;
            }
            _ if depth != 1 => continue,
            _ => {}
        }
        let d = r.data;
        if r.kind != rt::STRING && r.kind != rt::SHRFMLA && r.kind != rt::ARRAY && r.kind != rt::TABLE {
            pending_string = None;
        }
        match r.kind {
            rt::NUMBER => {
                if let (Some((pos, ixfe)), Some(n)) = (cell_at(d), f64_at(d, 6)) {
                    set(&mut sheet, wb, styles, budget, pos, ixfe, Value::number(n))?;
                }
            }
            rt::RK => {
                if let (Some((pos, ixfe)), Some(v)) = (cell_at(d), u32_at(d, 6)) {
                    set(&mut sheet, wb, styles, budget, pos, ixfe, Value::number(rk(v)))?;
                }
            }
            rt::MULRK => {
                let (Some(row), Some(c0)) = (u16_at(d, 0), u16_at(d, 2)) else { continue };
                let n = d.len().saturating_sub(6) / 6;
                for i in 0..n {
                    let at = 4 + i * 6;
                    let col = u32::from(c0) + i as u32;
                    if let (Some(ixfe), Some(v)) = (u16_at(d, at), u32_at(d, at + 2))
                        && col < MAX_COLS
                    {
                        set(&mut sheet, wb, styles, budget, CellRef::new(u32::from(row), col), ixfe, Value::number(rk(v)))?;
                    }
                }
            }
            rt::LABELSST => {
                if let (Some((pos, ixfe)), Some(i)) = (cell_at(d), u32_at(d, 6)) {
                    let v = g.sst.get(i as usize).map_or(Value::Empty, |s| Value::Text(s.clone()));
                    set(&mut sheet, wb, styles, budget, pos, ixfe, v)?;
                }
            }
            rt::LABEL => {
                if let Some((pos, ixfe)) = cell_at(d) {
                    let mut c = r.cur();
                    let text = c.skip(6).and_then(|_| c.xl_string()).unwrap_or_default();
                    set(&mut sheet, wb, styles, budget, pos, ixfe, Value::text(text))?;
                }
            }
            rt::BOOLERR => {
                if let (Some((pos, ixfe)), Some(&v), Some(&is_err)) = (cell_at(d), d.get(6), d.get(7)) {
                    let value = if is_err != 0 { Value::Error(error_value(v)) } else { Value::Bool(v != 0) };
                    set(&mut sheet, wb, styles, budget, pos, ixfe, value)?;
                }
            }
            rt::BLANK => {
                if let Some((pos, ixfe)) = cell_at(d)
                    && ixfe != DEFAULT_XF
                    && styles.get(ixfe, g, wb) != StyleId::DEFAULT
                {
                    set(&mut sheet, wb, styles, budget, pos, ixfe, Value::Empty)?;
                }
            }
            rt::MULBLANK => {
                let (Some(row), Some(c0)) = (u16_at(d, 0), u16_at(d, 2)) else { continue };
                let n = d.len().saturating_sub(6) / 2;
                for i in 0..n {
                    let col = u32::from(c0) + i as u32;
                    if let Some(ixfe) = u16_at(d, 4 + i * 2)
                        && col < MAX_COLS
                        && ixfe != DEFAULT_XF
                        && styles.get(ixfe, g, wb) != StyleId::DEFAULT
                    {
                        set(&mut sheet, wb, styles, budget, CellRef::new(u32::from(row), col), ixfe, Value::Empty)?;
                    }
                }
            }
            rt::FORMULA => {
                let Some((pos, ixfe)) = cell_at(d) else { continue };
                let Some(val) = d.get(6..14) else { continue };
                let value = if val.get(6..8) == Some(&[0xFF, 0xFF]) {
                    match val.first().copied().unwrap_or(0) {
                        0 => {
                            pending_string = Some(pos);
                            Value::Empty
                        }
                        1 => Value::Bool(val.get(2).is_some_and(|&b| b != 0)),
                        2 => Value::Error(error_value(val.get(2).copied().unwrap_or(0x2A))),
                        3 => Value::text(""),
                        _ => Value::Empty,
                    }
                } else {
                    f64_at(val, 0).map_or(Value::Empty, Value::number)
                };
                set(&mut sheet, wb, styles, budget, pos, ixfe, value)?;
                let mut c = r.cur();
                let Some(cce) = c.skip(20).and_then(|_| c.u16()).map(usize::from) else { continue };
                let mut rgce = Vec::with_capacity(cce.min(c.remaining()));
                for _ in 0..cce {
                    let Some(b) = c.u8() else { break };
                    rgce.push(b);
                }
                let mut rgcb = Vec::with_capacity(c.remaining());
                while let Some(b) = c.u8() {
                    rgcb.push(b);
                }
                match rgce.first() {
                    Some(0x01) => {
                        if let (Some(row), Some(col)) = (u16_at(&rgce, 1), u16_at(&rgce, 3)) {
                            deferred.push((pos, CellRef::new(u32::from(row), u32::from(col))));
                        }
                    }
                    Some(0x02) => formulas.tables += 1,
                    Some(_) => attach(&mut sheet, pos, &rgce, &rgcb, None, names, formulas),
                    None => {}
                }
            }
            rt::STRING => {
                if let Some(pos) = pending_string.take() {
                    let text = r.cur().xl_string().unwrap_or_default();
                    budget.text(text.len())?;
                    if let Some(cell) = sheet.cells.get_mut(pos) {
                        cell.value = Value::text(text);
                    }
                }
            }
            rt::SHRFMLA | rt::ARRAY => {
                let (Some(r0), Some(r1), Some(&c0), Some(&c1)) = (u16_at(d, 0), u16_at(d, 2), d.get(4), d.get(5)) else { continue };
                let range = RangeRef::new(CellRef::new(u32::from(r0), u32::from(c0)), CellRef::new(u32::from(r1), u32::from(c1)));
                let head = if r.kind == rt::SHRFMLA { 8 } else { 12 };
                let mut c = r.cur();
                let Some(cce) = c.skip(head).and_then(|_| c.u16()).map(usize::from) else { continue };
                let mut rgce = Vec::with_capacity(cce.min(c.remaining()));
                for _ in 0..cce {
                    let Some(b) = c.u8() else { break };
                    rgce.push(b);
                }
                let mut rgcb = Vec::with_capacity(c.remaining());
                while let Some(b) = c.u8() {
                    rgcb.push(b);
                }
                let map = if r.kind == rt::SHRFMLA { &mut shared } else { &mut arrays };
                if map.len() < MAX_CELLS {
                    map.insert(range.start, Body { range, rgce, rgcb });
                }
            }
            rt::TABLE => formulas.tables += 1,
            rt::ROW => {
                let (Some(row), Some(height), Some(flags)) = (u16_at(d, 0), u16_at(d, 6), u32_at(d, 12)) else { continue };
                if u32::from(row) >= MAX_ROWS {
                    continue;
                }
                let mut info = LineInfo {
                    custom: flags & 0x40 != 0,
                    hidden: flags & 0x20 != 0,
                    outline: (flags & 7) as u8,
                    collapsed: flags & 0x10 != 0,
                    ..Default::default()
                };
                let px = pt_to_px(f64::from(height & 0x7FFF) / 20.0);
                if px > 0.0 && (info.custom || (px - sheet.default_row_height).abs() > 0.5) {
                    info.size = Some(px);
                }
                if flags & 0x80 != 0 {
                    let ixfe = ((flags >> 16) & 0x0FFF) as u16;
                    if ixfe != DEFAULT_XF {
                        let id = styles.get(ixfe, g, wb);
                        if id != StyleId::DEFAULT {
                            info.style = Some(id);
                        }
                    }
                }
                if info != LineInfo::default() {
                    sheet.rows.insert(u32::from(row), info);
                }
            }
            rt::COLINFO => {
                let (Some(c0), Some(c1), Some(width), Some(ixfe), Some(flags)) =
                    (u16_at(d, 0), u16_at(d, 2), u16_at(d, 4), u16_at(d, 6), u16_at(d, 8))
                else {
                    continue;
                };
                if c0 > c1 || u32::from(c0) >= MAX_COLS {
                    continue;
                }
                let mut info =
                    LineInfo { hidden: flags & 1 != 0, outline: ((flags >> 8) & 7) as u8, collapsed: flags & 0x1000 != 0, ..Default::default() };
                let px = col_width_to_px(f64::from(width) / 256.0);
                if px > 0.0 && px != sheet.default_col_width {
                    info.size = Some(px);
                }
                if ixfe != DEFAULT_XF {
                    let id = styles.get(ixfe, g, wb);
                    if id != StyleId::DEFAULT {
                        info.style = Some(id);
                    }
                }
                if info == LineInfo::default() {
                    continue;
                }
                for col in u32::from(c0)..=u32::from(c1).min(MAX_COLS - 1) {
                    sheet.cols.insert(col, info);
                }
            }
            rt::DEFCOLWIDTH => {
                if let Some(chars) = u16_at(d, 0) {
                    let px = ((f64::from(chars.min(255)) * 7.0 + 5.0) / 8.0).ceil() * 8.0;
                    if px > 0.0 {
                        sheet.default_col_width = px as f32;
                    }
                }
            }
            rt::DEFAULTROWHEIGHT => {
                if let Some(twips) = u16_at(d, 2) {
                    let px = pt_to_px(f64::from(twips) / 20.0);
                    if px > 0.0 {
                        sheet.default_row_height = px;
                    }
                }
            }
            rt::MERGECELLS => {
                let n = usize::from(u16_at(d, 0).unwrap_or(0));
                for i in 0..n {
                    let at = 2 + i * 8;
                    let (Some(r0), Some(r1), Some(c0), Some(c1)) = (u16_at(d, at), u16_at(d, at + 2), u16_at(d, at + 4), u16_at(d, at + 6)) else {
                        break;
                    };
                    if r0 > r1 || c0 > c1 || u32::from(c1) >= MAX_COLS {
                        continue;
                    }
                    if sheet.merges.len() >= MAX_MERGES {
                        return Err(IoError::TooLarge("XLS exceeds 100000 merges".into()));
                    }
                    let m = RangeRef::new(CellRef::new(u32::from(r0), u32::from(c0)), CellRef::new(u32::from(r1), u32::from(c1)));
                    if !m.is_single() {
                        sheet.merges.push(m);
                    }
                }
            }
            rt::WINDOW2 => {
                let flags = u16_at(d, 0).unwrap_or(0);
                sheet.show_formulas = flags & 0x01 != 0;
                sheet.show_gridlines = flags & 0x02 != 0;
                sheet.show_headings = flags & 0x04 != 0;
                frozen = flags & 0x08 != 0;
                sheet.show_zeros = flags & 0x10 != 0;
                sheet.right_to_left = flags & 0x40 != 0;
                if let (Some(row), Some(col)) = (u16_at(d, 2), u16_at(d, 4))
                    && u32::from(col) < MAX_COLS
                {
                    sheet.view_top_left = CellRef::new(u32::from(row), u32::from(col));
                }
            }
            rt::PANE => {
                if let (true, Some(x), Some(y)) = (frozen, u16_at(d, 0), u16_at(d, 2))
                    && (x > 0 || y > 0)
                    && u32::from(x) < MAX_COLS
                {
                    sheet.freeze = Some((u32::from(y), u32::from(x)));
                }
            }
            rt::SCL => {
                if let (Some(n), Some(den)) = (u16_at(d, 0), u16_at(d, 2))
                    && den > 0
                {
                    sheet.zoom = (u32::from(n) * 100 / u32::from(den)).clamp(10, 400) as u16;
                }
            }
            rt::NOTE => report.warn("Comments (notes) in .xls files are not imported."),
            rt::HLINK => report.warn("Hyperlinks in .xls files are not imported."),
            rt::MSODRAWING | rt::OBJ => report.warn("Charts, pictures and drawings in .xls files are not imported."),
            rt::CONDFMT => report.warn("Conditional formatting in .xls files is not imported."),
            rt::DVAL => report.warn("Data validation in .xls files is not imported."),
            rt::AUTOFILTERINFO => report.warn("AutoFilters in .xls files are not imported."),
            _ => {}
        }
    }
    for (pos, anchor) in deferred {
        if let Some(a) = arrays.get(&anchor)
            && a.range.contains(pos)
        {
            if pos == a.range.start {
                attach(&mut sheet, pos, &a.rgce, &a.rgcb, Some(a.range), names, formulas);
            } else if let Some(cell) = sheet.cells.get_mut(pos) {
                // The anchor's array formula fills these cells.
                cell.value = Value::Empty;
            }
        } else if let Some(s) = shared.get(&anchor)
            && s.range.contains(pos)
        {
            attach(&mut sheet, pos, &s.rgce, &s.rgcb, None, names, formulas);
        } else {
            formulas.drop("a shared formula is missing".into());
        }
    }
    Ok(sheet)
}

/// Decodes a formula and puts it on the cell, or counts why it couldn't.
fn attach(sheet: &mut Sheet, pos: CellRef, rgce: &[u8], rgcb: &[u8], array: Option<RangeRef>, names: &Names, formulas: &mut FormulaStats) {
    match decode(rgce, rgcb, pos, names) {
        Ok(text) => {
            let mut f = Formula::new(&text);
            if f.expr.is_none() {
                formulas.drop("formula text GridCraft can't parse".into());
                return;
            }
            f.array = array.filter(|r| !r.is_single());
            if let Some(cell) = sheet.cells.get_mut(pos) {
                cell.formula = Some(Arc::new(f));
            }
        }
        Err(why) => formulas.drop(why),
    }
}

// ------------------------------------------------------------------ formulas

/// A function in the built-in table ([MS-XLS] 2.5.198.17 Ftab): name and fixed argument count
/// (`-1` when the count varies; such functions are always written with PtgFuncVar).
fn function(id: u16) -> Option<(&'static str, i8)> {
    FUNCTIONS.binary_search_by_key(&id, |f| f.0).ok().and_then(|i| FUNCTIONS.get(i)).map(|f| (f.1, f.2))
}

#[rustfmt::skip]
const FUNCTIONS: &[(u16, &str, i8)] = &[
    (0, "COUNT", -1), (1, "IF", -1), (2, "ISNA", 1), (3, "ISERROR", 1), (4, "SUM", -1), (5, "AVERAGE", -1),
    (6, "MIN", -1), (7, "MAX", -1), (8, "ROW", -1), (9, "COLUMN", -1), (10, "NA", 0), (11, "NPV", -1),
    (12, "STDEV", -1), (13, "DOLLAR", -1), (14, "FIXED", -1), (15, "SIN", 1), (16, "COS", 1), (17, "TAN", 1),
    (18, "ATAN", 1), (19, "PI", 0), (20, "SQRT", 1), (21, "EXP", 1), (22, "LN", 1), (23, "LOG10", 1),
    (24, "ABS", 1), (25, "INT", 1), (26, "SIGN", 1), (27, "ROUND", 2), (28, "LOOKUP", -1), (29, "INDEX", -1),
    (30, "REPT", 2), (31, "MID", 3), (32, "LEN", 1), (33, "VALUE", 1), (34, "TRUE", 0), (35, "FALSE", 0),
    (36, "AND", -1), (37, "OR", -1), (38, "NOT", 1), (39, "MOD", 2), (40, "DCOUNT", 3), (41, "DSUM", 3),
    (42, "DAVERAGE", 3), (43, "DMIN", 3), (44, "DMAX", 3), (45, "DSTDEV", 3), (46, "VAR", -1), (47, "DVAR", 3),
    (48, "TEXT", 2), (49, "LINEST", -1), (50, "TREND", -1), (51, "LOGEST", -1), (52, "GROWTH", -1),
    (56, "PV", -1), (57, "FV", -1), (58, "NPER", -1), (59, "PMT", -1), (60, "RATE", -1), (61, "MIRR", 3),
    (62, "IRR", -1), (63, "RAND", 0), (64, "MATCH", -1), (65, "DATE", 3), (66, "TIME", 3), (67, "DAY", 1),
    (68, "MONTH", 1), (69, "YEAR", 1), (70, "WEEKDAY", -1), (71, "HOUR", 1), (72, "MINUTE", 1), (73, "SECOND", 1),
    (74, "NOW", 0), (75, "AREAS", 1), (76, "ROWS", 1), (77, "COLUMNS", 1), (78, "OFFSET", -1), (82, "SEARCH", -1),
    (83, "TRANSPOSE", 1), (86, "TYPE", 1), (97, "ATAN2", 2), (98, "ASIN", 1), (99, "ACOS", 1), (100, "CHOOSE", -1),
    (101, "HLOOKUP", -1), (102, "VLOOKUP", -1), (105, "ISREF", 1), (109, "LOG", -1), (111, "CHAR", 1),
    (112, "LOWER", 1), (113, "UPPER", 1), (114, "PROPER", 1), (115, "LEFT", -1), (116, "RIGHT", -1),
    (117, "EXACT", 2), (118, "TRIM", 1), (119, "REPLACE", 4), (120, "SUBSTITUTE", -1), (121, "CODE", 1),
    (124, "FIND", -1), (125, "CELL", -1), (126, "ISERR", 1), (127, "ISTEXT", 1), (128, "ISNUMBER", 1),
    (129, "ISBLANK", 1), (130, "T", 1), (131, "N", 1), (140, "DATEVALUE", 1), (141, "TIMEVALUE", 1),
    (142, "SLN", 3), (143, "SYD", 4), (144, "DDB", -1), (148, "INDIRECT", -1), (162, "CLEAN", 1),
    (163, "MDETERM", 1), (164, "MINVERSE", 1), (165, "MMULT", 2), (167, "IPMT", -1), (168, "PPMT", -1),
    (169, "COUNTA", -1), (183, "PRODUCT", -1), (184, "FACT", 1), (189, "DPRODUCT", 3), (190, "ISNONTEXT", 1),
    (193, "STDEVP", -1), (194, "VARP", -1), (195, "DSTDEVP", 3), (196, "DVARP", 3), (197, "TRUNC", -1),
    (198, "ISLOGICAL", 1), (199, "DCOUNTA", 3), (204, "USDOLLAR", -1), (205, "FINDB", -1), (206, "SEARCHB", -1),
    (207, "REPLACEB", 4), (208, "LEFTB", -1), (209, "RIGHTB", -1), (210, "MIDB", 3), (211, "LENB", 1),
    (212, "ROUNDUP", 2), (213, "ROUNDDOWN", 2), (214, "ASC", 1), (215, "DBCS", 1), (216, "RANK", -1),
    (219, "ADDRESS", -1), (220, "DAYS360", -1), (221, "TODAY", 0), (222, "VDB", -1), (227, "MEDIAN", -1),
    (228, "SUMPRODUCT", -1), (229, "SINH", 1), (230, "COSH", 1), (231, "TANH", 1), (232, "ASINH", 1),
    (233, "ACOSH", 1), (234, "ATANH", 1), (235, "DGET", 3), (244, "INFO", 1), (247, "DB", -1),
    (252, "FREQUENCY", 2), (261, "ERROR.TYPE", 1), (269, "AVEDEV", -1), (270, "BETADIST", -1), (271, "GAMMALN", 1),
    (272, "BETAINV", -1), (273, "BINOMDIST", 4), (274, "CHIDIST", 2), (275, "CHIINV", 2), (276, "COMBIN", 2),
    (277, "CONFIDENCE", 3), (278, "CRITBINOM", 3), (279, "EVEN", 1), (280, "EXPONDIST", 3), (281, "FDIST", 3),
    (282, "FINV", 3), (283, "FISHER", 1), (284, "FISHERINV", 1), (285, "FLOOR", 2), (286, "GAMMADIST", 4),
    (287, "GAMMAINV", 3), (288, "CEILING", 2), (289, "HYPGEOMDIST", 4), (290, "LOGNORMDIST", 3), (291, "LOGINV", 3),
    (292, "NEGBINOMDIST", 3), (293, "NORMDIST", 4), (294, "NORMSDIST", 1), (295, "NORMINV", 3), (296, "NORMSINV", 1),
    (297, "STANDARDIZE", 3), (298, "ODD", 1), (299, "PERMUT", 2), (300, "POISSON", 3), (301, "TDIST", 3),
    (302, "WEIBULL", 4), (303, "SUMXMY2", 2), (304, "SUMX2MY2", 2), (305, "SUMX2PY2", 2), (306, "CHITEST", 2),
    (307, "CORREL", 2), (308, "COVAR", 2), (309, "FORECAST", 3), (310, "FTEST", 2), (311, "INTERCEPT", 2),
    (312, "PEARSON", 2), (313, "RSQ", 2), (314, "STEYX", 2), (315, "SLOPE", 2), (316, "TTEST", 4), (317, "PROB", -1),
    (318, "DEVSQ", -1), (319, "GEOMEAN", -1), (320, "HARMEAN", -1), (321, "SUMSQ", -1), (322, "KURT", -1),
    (323, "SKEW", -1), (324, "ZTEST", -1), (325, "LARGE", 2), (326, "SMALL", 2), (327, "QUARTILE", 2),
    (328, "PERCENTILE", 2), (329, "PERCENTRANK", -1), (330, "MODE", -1), (331, "TRIMMEAN", 2), (332, "TINV", 2),
    (336, "CONCATENATE", -1), (337, "POWER", 2), (342, "RADIANS", 1), (343, "DEGREES", 1), (344, "SUBTOTAL", -1),
    (345, "SUMIF", -1), (346, "COUNTIF", 2), (347, "COUNTBLANK", 1), (350, "ISPMT", 4), (351, "DATEDIF", 3),
    (352, "DATESTRING", 1), (353, "NUMBERSTRING", 2), (354, "ROMAN", -1), (358, "GETPIVOTDATA", -1),
    (359, "HYPERLINK", -1), (360, "PHONETIC", 1), (361, "AVERAGEA", -1), (362, "MAXA", -1), (363, "MINA", -1),
    (364, "STDEVPA", -1), (365, "VARPA", -1), (366, "STDEVA", -1), (367, "VARA", -1), (368, "BAHTTEXT", 1),
    (379, "RTD", -1),
];

/// Removes the future-function prefixes Excel writes for functions newer than the file format.
fn strip_prefix(name: &str) -> &str {
    let n = name.strip_prefix("_xlfn.").unwrap_or(name);
    n.strip_prefix("_xlws.").unwrap_or(n)
}

fn number_text(n: f64) -> String {
    if !n.is_finite() {
        return "0".into();
    }
    let a = n.abs();
    if a != 0.0 && !(1e-9..1e15).contains(&a) { format!("{n:E}") } else { format!("{n}") }
}

fn quote_text(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A reference's position: absolute in Ref/Area tokens, an offset from `base` in RefN/AreaN.
fn loc(row: u16, col: u16, base: Option<CellRef>) -> (u32, u32, bool, bool) {
    let row_rel = col & 0x8000 != 0;
    let col_rel = col & 0x4000 != 0;
    let (r, c) = match base {
        Some(b) => (
            if row_rel { (i64::from(b.row) + i64::from(row as i16)).rem_euclid(65_536) as u32 } else { u32::from(row) },
            if col_rel { (i64::from(b.col) + i64::from((col & 0xFF) as u8 as i8)).rem_euclid(256) as u32 } else { u32::from(col & 0x3FFF) },
        ),
        None => (u32::from(row), u32::from(col & 0x3FFF)),
    };
    (r.min(MAX_ROWS - 1), c.min(MAX_COLS - 1), !row_rel, !col_rel)
}

fn cell_text(row: u32, col: u32, row_abs: bool, col_abs: bool) -> String {
    format!("{}{}{}{}", if col_abs { "$" } else { "" }, col_to_letters(col), if row_abs { "$" } else { "" }, row + 1)
}

fn area_text(a: (u32, u32, bool, bool), b: (u32, u32, bool, bool)) -> String {
    let d = |x: bool| if x { "$" } else { "" };
    if a.0 == 0 && b.0 == 65_535 {
        // A whole-column reference in the 65,536-row grid.
        return format!("{}{}:{}{}", d(a.3), col_to_letters(a.1), d(b.3), col_to_letters(b.1));
    }
    if a.1 == 0 && b.1 == 255 {
        return format!("{}{}:{}{}", d(a.2), a.0 + 1, d(b.2), b.0 + 1);
    }
    format!("{}:{}", cell_text(a.0, a.1, a.2, a.3), cell_text(b.0, b.1, b.2, b.3))
}

/// The `Sheet!` prefix for a 3-D reference, or `None` for a deleted sheet (`#REF!`).
fn sheet_prefix(ixti: u16, n: &Names) -> Result<Option<String>, String> {
    let &(sup, first, last) = n.xti.get(usize::from(ixti)).ok_or("a 3-D reference points at a missing sheet list")?;
    let book = n.supbooks.get(usize::from(sup)).ok_or("a 3-D reference points at a missing workbook")?;
    if book.kind != SupKind::Internal {
        return Err("links to other workbooks".into());
    }
    if first < 0 || last < 0 {
        return Ok(None);
    }
    let a = n.sheets.get(first as usize).ok_or("a 3-D reference points at a missing sheet")?;
    let b = n.sheets.get(last as usize).ok_or("a 3-D reference points at a missing sheet")?;
    Ok(Some(if first == last {
        format!("{}!", gridcraft_formula::quote_sheet(a))
    } else if needs_quotes(a) || needs_quotes(b) {
        format!("'{}:{}'!", a.replace('\'', "''"), b.replace('\'', "''"))
    } else {
        format!("{a}:{b}!")
    }))
}

/// Turns a parsed expression (reverse-Polish tokens) back into formula text. `base` is the
/// formula's cell, for the relative tokens of shared formulas.
fn decode(rgce: &[u8], rgcb: &[u8], base: CellRef, n: &Names) -> Result<String, String> {
    let mut c = Cur::slice(rgce);
    let mut extra = Cur::slice(rgcb);
    let mut st: Vec<String> = Vec::new();
    let short = || "a truncated formula".to_string();
    while c.remaining() > 0 {
        let t = c.u8().ok_or_else(short)?;
        match t {
            0x01 | 0x02 => return Err("a misplaced shared-formula token".into()),
            0x03..=0x11 => {
                let b = st.pop().ok_or_else(short)?;
                let a = st.pop().ok_or_else(short)?;
                let op = match t {
                    0x03 => "+",
                    0x04 => "-",
                    0x05 => "*",
                    0x06 => "/",
                    0x07 => "^",
                    0x08 => "&",
                    0x09 => "<",
                    0x0A => "<=",
                    0x0B => "=",
                    0x0C => ">=",
                    0x0D => ">",
                    0x0E => "<>",
                    0x0F => " ",
                    0x10 => ",",
                    _ => ":",
                };
                st.push(format!("{a}{op}{b}"));
            }
            0x12..=0x15 => {
                let a = st.pop().ok_or_else(short)?;
                st.push(match t {
                    0x12 => format!("+{a}"),
                    0x13 => format!("-{a}"),
                    0x14 => format!("{a}%"),
                    _ => format!("({a})"),
                });
            }
            0x16 => st.push(String::new()),
            0x17 => st.push(quote_text(&c.short_string().ok_or_else(short)?)),
            0x19 => {
                let grbit = c.u8().ok_or_else(short)?;
                if grbit & 0x04 != 0 {
                    let n = usize::from(c.u16().ok_or_else(short)?);
                    c.skip((n + 1) * 2).ok_or_else(short)?;
                } else {
                    c.skip(2).ok_or_else(short)?;
                }
                if grbit & 0x10 != 0 {
                    let a = st.pop().ok_or_else(short)?;
                    st.push(format!("SUM({a})"));
                }
            }
            0x1C => st.push(error_text(c.u8().ok_or_else(short)?).into()),
            0x1D => st.push(if c.u8().ok_or_else(short)? != 0 { "TRUE" } else { "FALSE" }.into()),
            0x1E => st.push(c.u16().ok_or_else(short)?.to_string()),
            0x1F => st.push(number_text(c.f64().ok_or_else(short)?)),
            0x20..=0x7F => {
                let kind = (t & 0x1F) | 0x20;
                match kind {
                    0x20 => {
                        c.skip(7).ok_or_else(short)?;
                        st.push(array_constant(&mut extra)?);
                    }
                    0x21 | 0x22 => {
                        let (argc, id) = if kind == 0x21 {
                            let id = c.u16().ok_or_else(short)?;
                            let (_, fixed) = function(id).ok_or_else(|| format!("unknown function #{id}"))?;
                            if fixed < 0 {
                                return Err(format!("unknown function #{id}"));
                            }
                            (fixed as usize, id)
                        } else {
                            let argc = usize::from(c.u8().ok_or_else(short)? & 0x7F);
                            let tab = c.u16().ok_or_else(short)?;
                            if tab & 0x8000 != 0 {
                                return Err("macro commands".into());
                            }
                            (argc, tab & 0x7FFF)
                        };
                        if argc > st.len() {
                            return Err(short());
                        }
                        let args = st.split_off(st.len() - argc);
                        if id == 255 {
                            // A user-defined or add-in function: the first argument is its name.
                            let (name, rest) = args.split_first().ok_or_else(short)?;
                            st.push(format!("{}({})", strip_prefix(name), rest.join(",")));
                        } else {
                            let (name, _) = function(id).ok_or_else(|| format!("unknown function #{id}"))?;
                            st.push(format!("{name}({})", args.join(",")));
                        }
                    }
                    0x23 => {
                        let i = c.u32().ok_or_else(short)? as usize;
                        let name = n.names.get(i.wrapping_sub(1)).ok_or("a missing defined name")?;
                        st.push(strip_prefix(name).to_string());
                    }
                    0x24 | 0x2C => {
                        let (row, col) = (c.u16().ok_or_else(short)?, c.u16().ok_or_else(short)?);
                        let (r, cc, ra, ca) = loc(row, col, (kind == 0x2C).then_some(base));
                        st.push(cell_text(r, cc, ra, ca));
                    }
                    0x25 | 0x2D => {
                        let (r0, r1) = (c.u16().ok_or_else(short)?, c.u16().ok_or_else(short)?);
                        let (c0, c1) = (c.u16().ok_or_else(short)?, c.u16().ok_or_else(short)?);
                        let b = (kind == 0x2D).then_some(base);
                        st.push(area_text(loc(r0, c0, b), loc(r1, c1, b)));
                    }
                    0x26 => {
                        // PtgMemArea: the reference follows as ordinary tokens; its cached
                        // rectangles sit in the extra data.
                        c.skip(6).ok_or_else(short)?;
                        let count = usize::from(extra.u16().ok_or_else(short)?);
                        extra.skip(count * 8).ok_or_else(short)?;
                    }
                    0x27 | 0x28 => c.skip(6).ok_or_else(short)?,
                    0x29 | 0x2E | 0x2F => c.skip(2).ok_or_else(short)?,
                    0x2A => {
                        c.skip(4).ok_or_else(short)?;
                        st.push("#REF!".into());
                    }
                    0x2B => {
                        c.skip(8).ok_or_else(short)?;
                        st.push("#REF!".into());
                    }
                    0x39 => {
                        let ixti = c.u16().ok_or_else(short)?;
                        let i = c.u32().ok_or_else(short)? as usize;
                        let &(sup, _, _) = n.xti.get(usize::from(ixti)).ok_or("a missing external name")?;
                        let book = n.supbooks.get(usize::from(sup)).ok_or("a missing external name")?;
                        if book.kind == SupKind::External {
                            return Err("links to other workbooks".into());
                        }
                        let name = book.names.get(i.wrapping_sub(1)).ok_or("a missing external name")?;
                        st.push(strip_prefix(name).to_string());
                    }
                    0x3A | 0x3C => {
                        let ixti = c.u16().ok_or_else(short)?;
                        let (row, col) = (c.u16().ok_or_else(short)?, c.u16().ok_or_else(short)?);
                        st.push(match (kind, sheet_prefix(ixti, n)?) {
                            (0x3A, Some(p)) => {
                                let (r, cc, ra, ca) = loc(row, col, None);
                                format!("{p}{}", cell_text(r, cc, ra, ca))
                            }
                            _ => "#REF!".into(),
                        });
                    }
                    0x3B | 0x3D => {
                        let ixti = c.u16().ok_or_else(short)?;
                        let (r0, r1) = (c.u16().ok_or_else(short)?, c.u16().ok_or_else(short)?);
                        let (c0, c1) = (c.u16().ok_or_else(short)?, c.u16().ok_or_else(short)?);
                        st.push(match (kind, sheet_prefix(ixti, n)?) {
                            (0x3B, Some(p)) => format!("{p}{}", area_text(loc(r0, c0, None), loc(r1, c1, None))),
                            _ => "#REF!".into(),
                        });
                    }
                    _ => return Err(format!("unsupported formula token {t:#04x}")),
                }
            }
            _ => return Err(format!("unsupported formula token {t:#04x}")),
        }
        if st.len() > MAX_STACK || st.last().is_some_and(|s| s.len() > MAX_FORMULA) {
            return Err("a formula that is too long".into());
        }
    }
    match (st.pop(), st.is_empty()) {
        (Some(text), true) => Ok(text),
        _ => Err("an incomplete formula".into()),
    }
}

/// An array constant from the formula's extra data ([MS-XLS] 2.5.198.8 PtgExtraArray).
fn array_constant(extra: &mut Cur<'_>) -> Result<String, String> {
    let short = || "a truncated array constant".to_string();
    let cols = usize::from(extra.u8().ok_or_else(short)?) + 1;
    let rows = usize::from(extra.u16().ok_or_else(short)?) + 1;
    if rows.saturating_mul(cols) > MAX_ARRAY {
        return Err("an array constant that is too large".into());
    }
    let mut out = String::from("{");
    for r in 0..rows {
        if r > 0 {
            out.push(';');
        }
        for col in 0..cols {
            if col > 0 {
                out.push(',');
            }
            let v = match extra.u8().ok_or_else(short)? {
                0x01 => number_text(extra.f64().ok_or_else(short)?),
                0x02 => quote_text(&extra.xl_string().ok_or_else(short)?),
                0x04 => {
                    let b = extra.u8().ok_or_else(short)?;
                    extra.skip(7).ok_or_else(short)?;
                    if b != 0 { "TRUE" } else { "FALSE" }.to_string()
                }
                0x10 => {
                    let e = extra.u8().ok_or_else(short)?;
                    extra.skip(7).ok_or_else(short)?;
                    error_text(e).to_string()
                }
                _ => {
                    extra.skip(8).ok_or_else(short)?;
                    "0".to_string()
                }
            };
            out.push_str(&v);
            if out.len() > MAX_FORMULA {
                return Err("an array constant that is too large".into());
            }
        }
    }
    out.push('}');
    Ok(out)
}
