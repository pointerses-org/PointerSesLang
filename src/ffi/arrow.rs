//! Arrow-format shared-memory descriptors for zero-copy FFI between Pointerses
//! and Rust.
//!
//! Arrow describes columnar data through a schema (types + field names) plus
//! flat buffers. We use a compact Arrow-like layout: a schema header followed by
//! a contiguous, zero-copy payload. When a `.psp` file declares an `extern`
//! function, arguments are marshalled into an Arrow buffer and the target (a
//! Rust `cdylib`) receives a descriptor (pointer + length + type tag) with no
//! copying.

/// Arrow-like primitive type tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowType {
    Int,
    Float,
    Bool,
    Utf8,
    Struct,
}

impl ArrowType {
    pub fn tag(&self) -> u8 {
        match self {
            ArrowType::Int => 1,
            ArrowType::Float => 2,
            ArrowType::Bool => 3,
            ArrowType::Utf8 => 4,
            ArrowType::Struct => 5,
        }
    }
    pub fn from_tag(t: u8) -> ArrowType {
        match t {
            1 => ArrowType::Int,
            2 => ArrowType::Float,
            3 => ArrowType::Bool,
            4 => ArrowType::Utf8,
            _ => ArrowType::Struct,
        }
    }
}

/// A single field in an Arrow schema.
#[derive(Debug, Clone)]
pub struct ArrowField {
    pub name: String,
    pub ty: ArrowType,
}

/// An Arrow schema (ordered field list).
#[derive(Debug, Clone, Default)]
pub struct ArrowSchema {
    pub fields: Vec<ArrowField>,
}

impl ArrowSchema {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.fields.len() as u32).to_le_bytes());
        for f in &self.fields {
            out.extend_from_slice(&(f.name.len() as u32).to_le_bytes());
            out.extend_from_slice(f.name.as_bytes());
            out.push(f.ty.tag());
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<ArrowSchema, String> {
        let mut i = 0usize;
        let read_u32 = |buf: &[u8], i: &mut usize| -> u32 {
            let mut b = [0u8; 4];
            for j in 0..4 {
                b[j] = *buf.get(*i).unwrap_or(&0);
                *i += 1;
            }
            u32::from_le_bytes(b)
        };
        let n = read_u32(buf, &mut i);
        let mut fields = Vec::new();
        for _ in 0..n {
            let len = read_u32(buf, &mut i) as usize;
            let name = String::from_utf8_lossy(&buf[i..i + len]).to_string();
            i += len;
            let ty = ArrowType::from_tag(*buf.get(i).unwrap_or(&5));
            i += 1;
            fields.push(ArrowField { name, ty });
        }
        Ok(ArrowSchema { fields })
    }
}

/// A shared-memory descriptor handed across the FFI boundary: a pointer to the
/// payload plus its length and a schema tag.
#[derive(Debug, Clone, Copy)]
pub struct SharedDescriptor {
    pub ptr: *const u8,
    pub len: usize,
    pub schema_tag: u8,
}

impl SharedDescriptor {
    pub fn null() -> Self {
        SharedDescriptor { ptr: std::ptr::null(), len: 0, schema_tag: 0 }
    }
    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }
}

/// Build an Arrow record batch from a list of i64 values (the common FFI case).
/// Returns `(buffer, schema)` where the buffer is the zero-copy payload.
pub fn build_record_batch(values: &[i64], schema: &ArrowSchema) -> (Vec<u8>, ArrowSchema) {
    let mut payload = Vec::with_capacity(values.len() * 8);
    for v in values {
        payload.extend_from_slice(&v.to_le_bytes());
    }
    (payload, schema.clone())
}

/// Read a record batch payload back into i64 values.
pub fn read_record_batch(payload: &[u8], count: usize) -> Vec<i64> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let mut b = [0u8; 8];
        for j in 0..8 {
            b[j] = *payload.get(i * 8 + j).unwrap_or(&0);
        }
        out.push(i64::from_le_bytes(b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_roundtrip() {
        let s = ArrowSchema {
            fields: vec![
                ArrowField { name: "a".into(), ty: ArrowType::Int },
                ArrowField { name: "b".into(), ty: ArrowType::Int },
            ],
        };
        let enc = s.encode();
        let dec = ArrowSchema::decode(&enc).unwrap();
        assert_eq!(dec.fields.len(), 2);
        assert_eq!(dec.fields[0].name, "a");
    }
}

