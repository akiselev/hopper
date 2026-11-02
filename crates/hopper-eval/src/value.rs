//! Values: a type plus where its bytes are. Reading goes through a [`Session`], which owns the
//! memory handle for one evaluation and caches pages so a value tree reads each page once.

use std::collections::HashMap;
use std::sync::Arc;

use hopper_core::target::Memory;
use hopper_core::{HopperError, Reg, RegisterSet, Result};
use hopper_symbols::{Binary, DwarfIndex, ModuleId, RustcVersion, SymbolSet};

use crate::location::ValueLocation;
use crate::types::{BaseKind, Builtin, Field, Type, TypeCache, TypeId, TypeKind};

const PAGE: u64 = 4096;
/// Values larger than this are never read whole; their children are read one by one.
pub const MAX_WHOLE_READ: u64 = 1 << 20;

/// One evaluation's view of the debuggee: symbols, memory, the frame's registers.
pub struct Session<'a> {
    pub symbols: &'a SymbolSet,
    pub module: ModuleId,
    pub binary: Arc<Binary>,
    pub types: &'a TypeCache,
    pub regs: &'a RegisterSet,
    pub rustc: Option<RustcVersion>,
    memory: &'a mut dyn Memory,
    pages: HashMap<u64, Option<Arc<[u8]>>>,
}

impl std::fmt::Debug for Session<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("module", &self.module)
            .field("rustc", &self.rustc)
            .finish()
    }
}

impl<'a> Session<'a> {
    pub fn new(
        symbols: &'a SymbolSet,
        module: ModuleId,
        types: &'a TypeCache,
        memory: &'a mut dyn Memory,
        regs: &'a RegisterSet,
        rustc: Option<RustcVersion>,
    ) -> Result<Session<'a>> {
        let binary = symbols
            .module(module)
            .ok_or_else(|| HopperError::Invalid(format!("no module {module:?}")))?
            .binary()
            .clone();
        Ok(Session {
            symbols,
            module,
            binary,
            types,
            regs,
            rustc,
            memory,
            pages: HashMap::new(),
        })
    }

    pub fn dwarf(&self) -> Result<&DwarfIndex> {
        Ok(self.binary.dwarf()?.as_ref())
    }

    pub fn memory(&mut self) -> &mut dyn Memory {
        &mut *self.memory
    }

    pub fn ty(&self, id: &TypeId) -> Result<Arc<Type>> {
        self.types.get(&self.binary, id)
    }

    /// The type with typedefs and qualifiers removed.
    pub fn strip(&self, ty: &Arc<Type>) -> Result<Arc<Type>> {
        self.types.strip(&self.binary, ty.clone())
    }

    pub fn size_of(&self, id: &TypeId) -> Result<u64> {
        self.types.size_of(&self.binary, id)
    }

    /// Read `len` bytes at `addr`, through the page cache for small reads.
    pub fn read(&mut self, addr: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if len > MAX_WHOLE_READ {
            return Err(HopperError::Eval(format!(
                "refusing to read {len} bytes at once"
            )));
        }
        let first = addr / PAGE;
        let last = (addr + len - 1) / PAGE;
        if last - first <= 2 {
            let mut out = Vec::with_capacity(len as usize);
            let mut ok = true;
            for page in first..=last {
                match self.page(page) {
                    Some(bytes) => {
                        let start = if page == first {
                            (addr % PAGE) as usize
                        } else {
                            0
                        };
                        let end = if page == last {
                            ((addr + len - 1) % PAGE) as usize + 1
                        } else {
                            PAGE as usize
                        };
                        out.extend_from_slice(&bytes[start..end]);
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                return Ok(out);
            }
        }
        let mut buf = vec![0u8; len as usize];
        self.memory.read(addr, &mut buf)?;
        Ok(buf)
    }

    fn page(&mut self, page: u64) -> Option<Arc<[u8]>> {
        if let Some(p) = self.pages.get(&page) {
            return p.clone();
        }
        let mut buf = vec![0u8; PAGE as usize];
        let got = self
            .memory
            .read(page * PAGE, &mut buf)
            .ok()
            .map(|()| Arc::from(buf));
        self.pages.insert(page, got.clone());
        got
    }

    pub fn read_u64(&mut self, addr: u64) -> Result<u64> {
        let b = self.read(addr, 8)?;
        Ok(u64::from_le_bytes(b.try_into().expect("8 bytes")))
    }

    pub fn read_u8(&mut self, addr: u64) -> Result<u8> {
        Ok(self.read(addr, 1)?[0])
    }

    /// A value of `ty` at `addr`.
    pub fn at(&self, ty: &TypeId, addr: u64) -> Result<Value> {
        Ok(Value {
            ty: self.ty(ty)?,
            loc: ValueLocation::Memory { addr },
        })
    }

    /// A value made from known bytes (a literal, a computed result).
    pub fn from_bytes(&self, ty: &TypeId, bytes: Vec<u8>) -> Result<Value> {
        Ok(Value {
            ty: self.ty(ty)?,
            loc: ValueLocation::Bytes(Arc::from(bytes)),
        })
    }

    pub fn integer(&self, v: i64) -> Value {
        Value {
            ty: Arc::new(Type::builtin(Builtin::I64)),
            loc: ValueLocation::Bytes(Arc::from(v.to_le_bytes().to_vec())),
        }
    }
}

/// A value: its type and where its bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub ty: Arc<Type>,
    pub loc: ValueLocation,
}

impl Value {
    pub fn size(&self, s: &Session<'_>) -> Result<u64> {
        match self.ty.size {
            Some(n) => Ok(n),
            None => s.size_of(&self.ty.id),
        }
    }

    pub fn address(&self) -> Option<u64> {
        match self.loc {
            ValueLocation::Memory { addr } => Some(addr),
            _ => None,
        }
    }

    /// The value's bytes.
    pub fn bytes(&self, s: &mut Session<'_>) -> Result<Arc<[u8]>> {
        let size = self.size(s)?;
        self.bytes_prefix(s, size)
    }

    /// The first `len` bytes of the value.
    pub fn bytes_prefix(&self, s: &mut Session<'_>, len: u64) -> Result<Arc<[u8]>> {
        match &self.loc {
            ValueLocation::Memory { addr } => Ok(Arc::from(s.read(*addr, len)?)),
            ValueLocation::Register { reg } => {
                let b = s.regs.get_bytes(*reg).ok_or_else(|| {
                    HopperError::Eval(format!("register {} unavailable", reg.name()))
                })?;
                let mut v = b.to_vec();
                v.resize(len as usize, 0);
                Ok(Arc::from(v))
            }
            ValueLocation::Bytes(b) => {
                let mut v = b.to_vec();
                v.resize(len.max(b.len() as u64) as usize, 0);
                v.truncate(len as usize);
                Ok(Arc::from(v))
            }
            ValueLocation::OptimisedOut => Err(HopperError::OptimisedOut),
        }
    }

    /// The sub-value at `offset` of type `ty`.
    pub fn slice(&self, s: &mut Session<'_>, offset: u64, ty: &TypeId) -> Result<Value> {
        let ty = s.ty(ty)?;
        let loc = match &self.loc {
            ValueLocation::Memory { addr } => ValueLocation::Memory {
                addr: addr + offset,
            },
            ValueLocation::OptimisedOut => ValueLocation::OptimisedOut,
            ValueLocation::Register { .. } | ValueLocation::Bytes(_) => {
                let size = match ty.size {
                    Some(n) => n,
                    None => s.size_of(&ty.id)?,
                };
                let all = self.bytes(s)?;
                let start = (offset as usize).min(all.len());
                let end = ((offset + size) as usize).min(all.len());
                let mut v = all[start..end].to_vec();
                v.resize(size as usize, 0);
                ValueLocation::Bytes(Arc::from(v))
            }
        };
        Ok(Value { ty, loc })
    }

    /// A struct or union member.
    pub fn field(&self, s: &mut Session<'_>, f: &Field) -> Result<Value> {
        let v = self.slice(s, f.offset, &f.ty)?;
        let Some((bit, width)) = f.bits else {
            return Ok(v);
        };
        // Extract the bit field into a value of its own.
        let size = v.size(s)?.max(width.div_ceil(8));
        let span = (bit + width).div_ceil(8);
        let raw = self
            .slice(s, f.offset, &f.ty)
            .and_then(|x| x.bytes_prefix(s, span.max(size)))?;
        let mut word = 0u128;
        for (i, b) in raw.iter().take(16).enumerate() {
            word |= (*b as u128) << (8 * i);
        }
        let mask = if width >= 128 {
            u128::MAX
        } else {
            (1u128 << width) - 1
        };
        let x = (word >> bit) & mask;
        let bytes = x.to_le_bytes()[..size as usize].to_vec();
        Ok(Value {
            ty: v.ty,
            loc: ValueLocation::Bytes(Arc::from(bytes)),
        })
    }

    /// The value a pointer value points at, with type `pointee`.
    pub fn pointee(&self, s: &mut Session<'_>, pointee: &TypeId) -> Result<Value> {
        let addr = self.as_u64(s)?;
        s.at(pointee, addr)
    }

    /// The value as an unsigned integer (pointers, integers, bools, chars, C enums), zero-extended.
    pub fn as_u64(&self, s: &mut Session<'_>) -> Result<u64> {
        let size = self.size(s)?.min(8);
        let b = self.bytes_prefix(s, size)?;
        let mut buf = [0u8; 8];
        buf[..b.len().min(8)].copy_from_slice(&b[..b.len().min(8)]);
        Ok(u64::from_le_bytes(buf))
    }

    /// The value as a signed integer when its type is signed, else zero-extended.
    pub fn as_i128(&self, s: &mut Session<'_>) -> Result<i128> {
        let size = self.size(s)?.min(16);
        let b = self.bytes_prefix(s, size)?;
        let mut buf = [0u8; 16];
        buf[..b.len()].copy_from_slice(&b);
        let u = u128::from_le_bytes(buf);
        let stripped = s.strip(&self.ty)?;
        let signed = matches!(
            stripped.kind,
            TypeKind::Base(BaseKind::Signed | BaseKind::SignedChar)
        ) || matches!(&stripped.kind, TypeKind::CEnum(e) if e.signed);
        if signed && size > 0 && size < 16 {
            let shift = 128 - size * 8;
            Ok(((u << shift) as i128) >> shift)
        } else {
            Ok(u as i128)
        }
    }

    /// The register the value lives in, if it is exactly one register.
    pub fn register(&self) -> Option<Reg> {
        match self.loc {
            ValueLocation::Register { reg } => Some(reg),
            _ => None,
        }
    }
}
