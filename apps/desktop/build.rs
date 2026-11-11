// SPDX-License-Identifier: EUPL-1.2
//! Compiles the application icon into nuschel.exe; other targets do nothing.
//!
//! Explorer shows an executable's first icon group, and GPUI loads icon group 1
//! for the window class (title bar, taskbar, Alt+Tab). No resource compiler is
//! needed: this writes the COFF object that windres produces for
//! `1 ICON "assets/nuschel.ico"`, one RT_ICON per image plus the RT_GROUP_ICON
//! directory in a `.rsrc` section, and links it into the program.
use std::{env, fs, path::PathBuf};

const ICON: &str = "assets/nuschel.ico";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    let cfg = |key: &str| env::var(format!("CARGO_CFG_TARGET_{key}")).unwrap_or_default();
    if cfg("OS") != "windows" {
        return Ok(());
    }
    println!("cargo:rerun-if-changed={ICON}");
    // GNU ld merges `.rsrc` sections of objects, as windres writes them;
    // link.exe and lld expect the split cvtres layout instead.
    let machine = match (cfg("ENV").as_str(), cfg("ARCH").as_str()) {
        ("gnu", "x86_64") => Machine::Amd64,
        ("gnu", "x86") => Machine::I386,
        (env, arch) => {
            println!("cargo:warning=no application icon for {arch}-{env} Windows builds");
            return Ok(());
        }
    };
    let icon = match fs::read(ICON) {
        Ok(icon) => icon,
        // The crane builds in flake.nix filter sources by file name extension;
        // a tree without the icon builds a program without it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("cargo:warning={ICON} is missing; nuschel.exe has no icon");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let object = resource_object(&icon, machine).map_err(|error| format!("{ICON}: {error}"))?;
    let path = PathBuf::from(env::var("OUT_DIR")?).join("nuschel-icon.o");
    fs::write(&path, object)?;
    println!("cargo:rustc-link-arg-bins={}", path.display());
    Ok(())
}

#[derive(Clone, Copy)]
enum Machine {
    Amd64,
    I386,
}

impl Machine {
    fn id(self) -> u16 {
        match self {
            Machine::Amd64 => 0x8664,
            Machine::I386 => 0x014c,
        }
    }

    fn characteristics(self) -> u16 {
        const LINE_NUMS_STRIPPED: u16 = 0x0004;
        const BIT32_MACHINE: u16 = 0x0100;
        match self {
            Machine::Amd64 => LINE_NUMS_STRIPPED,
            Machine::I386 => LINE_NUMS_STRIPPED | BIT32_MACHINE,
        }
    }

    /// The relocation that stores an address relative to the image base (RVA).
    fn rva_relocation(self) -> u16 {
        match self {
            Machine::Amd64 => 0x0003, // IMAGE_REL_AMD64_ADDR32NB
            Machine::I386 => 0x0007,  // IMAGE_REL_I386_DIR32NB
        }
    }
}

const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;
/// windres's default language, U.S. English.
const LANGUAGE: u32 = 0x0409;
const DIRECTORY: usize = 16;
const DIRECTORY_ENTRY: usize = 8;
const DATA_ENTRY: usize = 16;
const SUBDIRECTORY: u32 = 0x8000_0000;

fn resource_object(icon: &[u8], machine: Machine) -> Result<Vec<u8>, String> {
    let u16_at = |at: usize| {
        icon.get(at..at + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    // ICONDIR: reserved 0, type 1 (icon), image count, then 16-byte entries.
    if u16_at(0) != Some(0) || u16_at(2) != Some(1) {
        return Err("not an icon file".into());
    }
    let count = u16_at(4).filter(|count| *count > 0).ok_or("no images")?;
    let mut images = Vec::new();
    // GRPICONDIR, then each ICONDIRENTRY with its RT_ICON id in place of
    // the image's file offset (GRPICONDIRENTRY).
    let mut group = [0u16, 1, count]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    for index in 0..usize::from(count) {
        let start = 6 + 16 * index;
        let entry = icon.get(start..start + 16).ok_or("truncated directory")?;
        let word = |at: usize| {
            u32::from_le_bytes([entry[at], entry[at + 1], entry[at + 2], entry[at + 3]])
        };
        let (length, offset) = (word(8) as usize, word(12) as usize);
        let image = offset
            .checked_add(length)
            .and_then(|end| icon.get(offset..end))
            .ok_or("image outside the file")?;
        images.push((index as u32 + 1, image));
        group.extend_from_slice(&entry[..12]);
        group.extend_from_slice(&(index as u16 + 1).to_le_bytes());
    }
    let types = [
        (RT_ICON, images),
        (RT_GROUP_ICON, vec![(1, group.as_slice())]),
    ];
    let leaves = types
        .iter()
        .flat_map(|(_, names)| names.iter().map(|(_, data)| *data))
        .collect::<Vec<_>>();

    // windres's layout: the type directory, then per type its name directory
    // followed by one language directory per name; the data entries; the data,
    // each aligned to eight bytes.
    let mut offset = DIRECTORY + DIRECTORY_ENTRY * types.len();
    let mut name_directories = Vec::new();
    let mut language_directories = Vec::new();
    for (_, names) in &types {
        name_directories.push(offset);
        offset += DIRECTORY + DIRECTORY_ENTRY * names.len();
        for _ in names {
            language_directories.push(offset);
            offset += DIRECTORY + DIRECTORY_ENTRY;
        }
    }
    let data_entries = offset;
    offset += DATA_ENTRY * leaves.len();
    let mut data_offsets = Vec::new();
    for data in &leaves {
        offset = offset.next_multiple_of(8);
        data_offsets.push(offset);
        offset += data.len();
    }
    let length = offset.next_multiple_of(8);
    let narrow = |value: usize| u32::try_from(value).map_err(|_| "icon too large".to_string());

    let mut section = Vec::with_capacity(length);
    let directory = |section: &mut Vec<u8>, entries: usize| {
        section.extend_from_slice(&[0; 12]); // characteristics, time stamp, version
        put16(section, 0); // named entries
        put16(section, entries as u16); // id entries
    };
    directory(&mut section, types.len());
    for ((kind, _), at) in types.iter().zip(&name_directories) {
        put32(&mut section, *kind);
        put32(&mut section, narrow(*at)? | SUBDIRECTORY);
    }
    let mut leaf = 0;
    for (_, names) in &types {
        directory(&mut section, names.len());
        for (index, (id, _)) in names.iter().enumerate() {
            put32(&mut section, *id);
            put32(
                &mut section,
                narrow(language_directories[leaf + index])? | SUBDIRECTORY,
            );
        }
        for _ in names {
            directory(&mut section, 1);
            put32(&mut section, LANGUAGE);
            put32(&mut section, narrow(data_entries + DATA_ENTRY * leaf)?);
            leaf += 1;
        }
    }
    let mut relocations = Vec::new();
    for (data, at) in leaves.iter().zip(&data_offsets) {
        // The linker adds the section's address to this offset.
        relocations.push(narrow(section.len())?);
        put32(&mut section, narrow(*at)?);
        put32(&mut section, narrow(data.len())?);
        put32(&mut section, 0); // code page
        put32(&mut section, 0); // reserved
    }
    for (data, at) in leaves.iter().zip(&data_offsets) {
        section.resize(*at, 0);
        section.extend_from_slice(data);
    }
    section.resize(length, 0);

    // COFF: file header, one section header, the section, its relocations,
    // a symbol for the section, and an empty string table.
    const FILE_HEADER: usize = 20;
    const SECTION_HEADER: usize = 40;
    const RELOCATION: usize = 10;
    let raw_data = FILE_HEADER + SECTION_HEADER;
    let relocations_at = raw_data + section.len();
    let symbols_at = relocations_at + RELOCATION * relocations.len();
    let relocation_count =
        u16::try_from(relocations.len()).map_err(|_| "too many images".to_string())?;
    let mut object = Vec::with_capacity(symbols_at + 22);
    put16(&mut object, machine.id());
    put16(&mut object, 1); // sections
    put32(&mut object, 0); // time stamp
    put32(&mut object, narrow(symbols_at)?);
    put32(&mut object, 1); // symbols
    put16(&mut object, 0); // optional header size
    put16(&mut object, machine.characteristics());
    object.extend_from_slice(b".rsrc\0\0\0");
    put32(&mut object, 0); // virtual size
    put32(&mut object, 0); // virtual address
    put32(&mut object, narrow(section.len())?);
    put32(&mut object, narrow(raw_data)?);
    put32(&mut object, narrow(relocations_at)?);
    put32(&mut object, 0); // line numbers
    put16(&mut object, relocation_count);
    put16(&mut object, 0); // line number count
    // IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_ALIGN_4BYTES | IMAGE_SCN_MEM_READ
    put32(&mut object, 0x4030_0040);
    object.extend_from_slice(&section);
    for at in relocations {
        put32(&mut object, at);
        put32(&mut object, 0); // symbol index: `.rsrc`
        put16(&mut object, machine.rva_relocation());
    }
    object.extend_from_slice(b".rsrc\0\0\0");
    put32(&mut object, 0); // value
    put16(&mut object, 1); // section number
    put16(&mut object, 0); // type
    object.extend_from_slice(&[3, 0]); // IMAGE_SYM_CLASS_STATIC, no auxiliary records
    put32(&mut object, 4); // string table size
    Ok(object)
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
