//! Target descriptions and the register layout of the `g` packet.
//!
//! gdb numbers registers as the description lists them: a `<reg>` without `regnum` takes the
//! previous number plus one (the first takes 0). The `g` packet holds the registers in regnum
//! order, each `bitsize / 8` bytes, little-endian on x86; a register whose offset lies past the
//! end of a `g` reply is read with `p`. rr builds its `g` reply exactly this way
//! (`GdbServer::target_registers`, every register in regnum order).

use hopper_core::{HopperError, Reg, RegisterSet, Result};

use crate::packet::from_hex;
use crate::xml::elements;

/// One register of the remote target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterDesc {
    pub name: String,
    pub regnum: u32,
    pub bitsize: u32,
    /// Byte offset in the `g` packet.
    pub offset: usize,
    /// The DWARF register hopper models under this name, if any.
    pub reg: Option<Reg>,
}

impl RegisterDesc {
    pub fn size(&self) -> usize {
        self.bitsize.div_ceil(8) as usize
    }
}

/// The parsed description.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TargetDescription {
    pub architecture: Option<String>,
    /// Registers in regnum order.
    pub registers: Vec<RegisterDesc>,
}

impl TargetDescription {
    /// Parse a description whose `xi:include`s are resolved by `fetch` (the client passes a
    /// function that reads the annex with `qXfer:features:read`). Includes nest; a cycle is an
    /// error.
    pub fn parse_with_includes(
        root: &str,
        fetch: &mut dyn FnMut(&str) -> Result<String>,
    ) -> Result<TargetDescription> {
        let mut regs: Vec<(String, u32, u32)> = Vec::new();
        let mut architecture = None;
        let mut next_regnum = 0u32;
        let mut stack: Vec<String> = Vec::new();
        Self::walk(
            root,
            fetch,
            &mut regs,
            &mut architecture,
            &mut next_regnum,
            &mut stack,
        )?;
        Self::from_regs(architecture, regs)
    }

    /// Parse a self-contained description; any `xi:include` is an error.
    pub fn parse(doc: &str) -> Result<TargetDescription> {
        Self::parse_with_includes(doc, &mut |href| {
            Err(HopperError::Protocol(format!(
                "unresolved xi:include {href:?}"
            )))
        })
    }

    fn walk(
        doc: &str,
        fetch: &mut dyn FnMut(&str) -> Result<String>,
        regs: &mut Vec<(String, u32, u32)>,
        architecture: &mut Option<String>,
        next_regnum: &mut u32,
        stack: &mut Vec<String>,
    ) -> Result<()> {
        for el in elements(doc)? {
            match el.name.as_str() {
                "architecture" => *architecture = Some(el.text.clone()),
                "xi:include" => {
                    let href = el
                        .attr("href")
                        .ok_or_else(|| HopperError::Protocol("xi:include without href".into()))?;
                    if stack.iter().any(|h| h == href) || stack.len() > 16 {
                        return Err(HopperError::Protocol(format!(
                            "xi:include cycle or depth limit at {href:?}"
                        )));
                    }
                    let sub = fetch(href)?;
                    stack.push(href.to_string());
                    Self::walk(&sub, fetch, regs, architecture, next_regnum, stack)?;
                    stack.pop();
                }
                "reg" => {
                    let name = el
                        .attr("name")
                        .ok_or_else(|| HopperError::Protocol("<reg> without name".into()))?;
                    let bitsize: u32 =
                        el.attr("bitsize")
                            .and_then(|b| b.parse().ok())
                            .ok_or_else(|| {
                                HopperError::Protocol(format!("<reg {name}> without a bitsize"))
                            })?;
                    let regnum = match el.attr("regnum") {
                        Some(n) => n.parse().map_err(|_| {
                            HopperError::Protocol(format!("<reg {name}> bad regnum {n:?}"))
                        })?,
                        None => *next_regnum,
                    };
                    *next_regnum = regnum + 1;
                    regs.push((name.to_string(), regnum, bitsize));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn from_regs(
        architecture: Option<String>,
        mut regs: Vec<(String, u32, u32)>,
    ) -> Result<TargetDescription> {
        regs.sort_by_key(|(_, n, _)| *n);
        for pair in regs.windows(2) {
            if pair[0].1 == pair[1].1 {
                return Err(HopperError::Protocol(format!(
                    "registers {} and {} share regnum {}",
                    pair[0].0, pair[1].0, pair[0].1
                )));
            }
        }
        let mut offset = 0;
        let registers = regs
            .into_iter()
            .map(|(name, regnum, bitsize)| {
                let d = RegisterDesc {
                    reg: Reg::from_name(&name),
                    name,
                    regnum,
                    bitsize,
                    offset,
                };
                offset += d.size();
                d
            })
            .collect();
        Ok(TargetDescription {
            architecture,
            registers,
        })
    }

    pub fn by_name(&self, name: &str) -> Option<&RegisterDesc> {
        self.registers.iter().find(|r| r.name == name)
    }

    pub fn by_reg(&self, reg: Reg) -> Option<&RegisterDesc> {
        self.registers.iter().find(|r| r.reg == Some(reg))
    }

    /// Decode a `g` reply (hex, already run-length expanded). Registers the reply covers and
    /// hopper models go in `values`; the raw bytes go in `raw`. A register sent as `xx`
    /// (unavailable) decodes as zero bytes in `raw` and is left out of `values`.
    pub fn decode_g(&self, reply: &[u8]) -> Result<RegisterSet> {
        if !reply.len().is_multiple_of(2) {
            return Err(HopperError::Protocol("odd-length g reply".into()));
        }
        let mut raw = Vec::with_capacity(reply.len() / 2);
        let mut available = Vec::with_capacity(reply.len() / 2);
        for pair in reply.chunks(2) {
            if pair == b"xx" {
                raw.push(0);
                available.push(false);
            } else {
                raw.push(from_hex(pair)?[0]);
                available.push(true);
            }
        }
        let mut set = RegisterSet {
            values: Vec::new(),
            raw: Vec::new(),
        };
        for d in &self.registers {
            let Some(reg) = d.reg else { continue };
            let end = d.offset + d.size();
            if end > raw.len() || !available[d.offset..end].iter().all(|a| *a) {
                continue;
            }
            set.set_bytes(reg, raw[d.offset..end].to_vec());
        }
        set.raw = raw;
        Ok(set)
    }

    /// Modelled registers that a `g` reply of `g_len` bytes does not cover.
    pub fn beyond_g(&self, g_len: usize) -> impl Iterator<Item = &RegisterDesc> {
        self.registers
            .iter()
            .filter(move |d| d.reg.is_some() && d.offset + d.size() > g_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORE: &str = r#"<feature name="org.gnu.gdb.i386.core">
  <reg name="rax" bitsize="64" type="int64"/>
  <reg name="rbx" bitsize="64" type="int64"/>
  <reg name="rip" bitsize="64" type="code_ptr" regnum="16"/>
  <reg name="eflags" bitsize="32" type="i386_eflags"/>
  <reg name="st0" bitsize="80" type="i387_ext"/>
</feature>"#;
    const SEG: &str = r#"<feature name="org.gnu.gdb.i386.segments">
  <reg name="fs_base" bitsize="64" type="data_ptr"/>
</feature>"#;

    fn sample() -> TargetDescription {
        let root = r#"<target><architecture>i386:x86-64</architecture>
<xi:include href="64bit-core.xml"/><xi:include href="64bit-seg.xml"/></target>"#;
        TargetDescription::parse_with_includes(root, &mut |href| match href {
            "64bit-core.xml" => Ok(CORE.into()),
            "64bit-seg.xml" => Ok(SEG.into()),
            _ => Err(HopperError::NotFound(href.into())),
        })
        .unwrap()
    }

    #[test]
    fn numbers_and_offsets() {
        let d = sample();
        assert_eq!(d.architecture.as_deref(), Some("i386:x86-64"));
        let rows: Vec<(&str, u32, usize)> = d
            .registers
            .iter()
            .map(|r| (r.name.as_str(), r.regnum, r.offset))
            .collect();
        assert_eq!(
            rows,
            [
                ("rax", 0, 0),
                ("rbx", 1, 8),
                ("rip", 16, 16),
                ("eflags", 17, 24),
                ("st0", 18, 28),
                ("fs_base", 19, 38)
            ]
        );
        assert_eq!(d.by_name("rbx").unwrap().reg, Some(Reg::RBX));
        assert_eq!(d.by_name("st0").unwrap().reg, None);
        assert_eq!(d.by_reg(Reg::RFLAGS).unwrap().name, "eflags");
    }

    #[test]
    fn decodes_g_with_unavailable_and_short_reply() {
        let d = sample();
        let mut g = String::new();
        g += "0100000000000000"; // rax
        g += "xxxxxxxxxxxxxxxx"; // rbx unavailable
        g += "0010400000000000"; // rip
        g += "46020000"; // eflags
        g += &"00".repeat(10); // st0
        // fs_base missing: the reply stops short.
        let set = d.decode_g(g.as_bytes()).unwrap();
        assert_eq!(set.get(Reg::RAX), Some(1));
        assert_eq!(set.get(Reg::RBX), None);
        assert_eq!(set.pc(), Some(0x401000));
        assert_eq!(set.get(Reg::RFLAGS), Some(0x246));
        assert_eq!(set.get(Reg::FS_BASE), None);
        assert_eq!(set.raw.len(), 38);
        let beyond: Vec<&str> = d.beyond_g(38).map(|r| r.name.as_str()).collect();
        assert_eq!(beyond, ["fs_base"]);
    }

    #[test]
    fn rejects_cycles_and_duplicates() {
        let err =
            TargetDescription::parse_with_includes(r#"<xi:include href="a.xml"/>"#, &mut |_| {
                Ok(r#"<xi:include href="a.xml"/>"#.into())
            });
        assert!(err.is_err());
        let dup = TargetDescription::parse(
            r#"<reg name="a" bitsize="8" regnum="3"/><reg name="b" bitsize="8" regnum="3"/>"#,
        );
        assert!(dup.is_err());
        assert!(TargetDescription::parse(r#"<xi:include href="x.xml"/>"#).is_err());
        assert!(TargetDescription::parse(r#"<reg name="a"/>"#).is_err());
    }
}
