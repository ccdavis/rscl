//! Source positions. The reader records (file, line, col) for every list, vector,
//! map, set and symbol it produces, as a small id in `ObjHeader::pos` pointing into
//! this table. Zero means "unknown".
use std::sync::{OnceLock, RwLock};

use crate::value::{ObjHeader, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SrcPos {
    pub file: u32,
    pub line: u32,
    pub col: u32,
}

struct Tables {
    positions: Vec<SrcPos>,
    files: Vec<String>,
}

fn tables() -> &'static RwLock<Tables> {
    static T: OnceLock<RwLock<Tables>> = OnceLock::new();
    T.get_or_init(|| {
        RwLock::new(Tables {
            positions: Vec::new(),
            files: vec!["<unknown>".to_string()],
        })
    })
}

pub fn intern_file(name: &str) -> u32 {
    let mut t = tables().write().unwrap();
    if let Some(i) = t.files.iter().position(|f| f == name) {
        return i as u32;
    }
    t.files.push(name.to_string());
    (t.files.len() - 1) as u32
}

pub fn file_name(id: u32) -> String {
    tables()
        .read()
        .unwrap()
        .files
        .get(id as usize)
        .cloned()
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// Record a position and return its id (never 0).
pub fn add(p: SrcPos) -> u32 {
    let mut t = tables().write().unwrap();
    t.positions.push(p);
    t.positions.len() as u32
}

pub fn get(id: u32) -> Option<SrcPos> {
    if id == 0 {
        return None;
    }
    tables()
        .read()
        .unwrap()
        .positions
        .get(id as usize - 1)
        .copied()
}

/// Attach a position id to a freshly created heap value (the reader owns it, so the
/// unsynchronised write is sound).
pub fn set_pos(v: &Value, id: u32) {
    if v.is_ptr() {
        unsafe { (*(v.ptr() as *mut ObjHeader)).pos = id }
    }
}

pub fn pos_id(v: &Value) -> u32 {
    v.header().map_or(0, |h| h.pos)
}

pub fn pos_of(v: &Value) -> Option<SrcPos> {
    get(pos_id(v))
}

/// `file:line:col`, or `<unknown>` for id 0.
pub fn describe(id: u32) -> String {
    match get(id) {
        Some(p) => format!("{}:{}:{}", file_name(p.file), p.line, p.col),
        None => "<unknown>".to_string(),
    }
}
