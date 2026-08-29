//! Human-readable rendering of what the reader and lifter produced.
//!
//! This is the transpiler's first observable behaviour and stays useful for
//! debugging every later stage: if the dump shows an operand as a number
//! rather than a symbol, lifting went wrong before translation ever ran.

use std::fmt::Write;

use iced_x86::{Formatter, IntelFormatter};

use crate::lifter::{LiftedFunction, LiftedInstruction, SymbolReference};
use crate::reader::{ObjectFile, SectionRole, SymbolBinding, SymbolRole};

pub fn dump_object(object: &ObjectFile, functions: &[LiftedFunction]) -> String {
    let mut output = String::new();
    dump_sections(&mut output, object);
    output.push('\n');
    dump_symbols(&mut output, object);
    output.push('\n');
    dump_relocations(&mut output, object);
    output.push('\n');
    for function in functions {
        dump_function(&mut output, object, function);
        output.push('\n');
    }
    output
}

fn dump_sections(output: &mut String, object: &ObjectFile) {
    writeln!(output, "sections:").unwrap();
    for (index, section) in object.sections.iter().enumerate() {
        let role = match section.role {
            SectionRole::Text => "text",
            SectionRole::Data => "data",
            SectionRole::ReadOnlyData => "rodata",
            SectionRole::ZeroFilled => "bss",
            SectionRole::Untranslated => "-",
        };
        writeln!(
            output,
            "  [{index:2}] {:<20} {role:<8} size {:#x} align {} relocations {}",
            section.name,
            section.size,
            section.alignment,
            section.relocations.len(),
        )
        .unwrap();
    }
}

fn dump_symbols(output: &mut String, object: &ObjectFile) {
    writeln!(output, "symbols:").unwrap();
    for (index, symbol) in object.symbols.iter().enumerate() {
        if symbol.name.is_empty() && symbol.role == SymbolRole::Other {
            continue;
        }
        let role = match symbol.role {
            SymbolRole::Function => "function",
            SymbolRole::Data => "data",
            SymbolRole::Section => "section",
            SymbolRole::Other => "other",
        };
        let binding = match symbol.binding {
            SymbolBinding::Local => "local",
            SymbolBinding::Global => "global",
            SymbolBinding::Weak => "weak",
        };
        let location = match symbol.section {
            Some(section) => format!("{}+{:#x}", object.sections[section].name, symbol.offset),
            None => "(undefined)".to_string(),
        };
        writeln!(
            output,
            "  [{index:2}] {:<24} {role:<9} {binding:<7} {location} size {:#x}",
            symbol.name, symbol.size,
        )
        .unwrap();
    }
}

fn dump_relocations(output: &mut String, object: &ObjectFile) {
    writeln!(output, "relocations:").unwrap();
    for section in &object.sections {
        for relocation in &section.relocations {
            writeln!(
                output,
                "  {}+{:#06x} {:?} -> {} {:+}",
                section.name,
                relocation.offset,
                relocation.kind,
                symbol_name(object, relocation.symbol),
                relocation.addend,
            )
            .unwrap();
        }
    }
}

fn dump_function(output: &mut String, object: &ObjectFile, function: &LiftedFunction) {
    writeln!(
        output,
        "function {} ({:#x}..{:#x}):",
        function.name,
        function.offset,
        function.offset + function.size
    )
    .unwrap();

    if !function.jump_tables.is_empty() {
        for table in function.jump_tables.values() {
            writeln!(
                output,
                "  jump table at {}+{:#x}: {} arms, {}-byte entries {}",
                object.sections[table.table_section].name,
                table.table_offset,
                table.targets.len(),
                table.stride,
                match table.base {
                    Some(base) => std::format!("measured from {base:#x}"),
                    None => "holding whole addresses".to_string(),
                },
            )
            .unwrap();
        }
    }

    let mut formatter = IntelFormatter::new();
    formatter.options_mut().set_first_operand_char_index(8);
    // Show `[rip+N]` rather than iced's resolved absolute address: our
    // instruction pointers are section offsets, so an absolute address here
    // would be a fiction.
    formatter.options_mut().set_rip_relative_addresses(true);
    formatter
        .options_mut()
        .set_space_after_operand_separator(true);

    for (position, lifted) in function.instructions.iter().enumerate() {
        let mut text = String::new();
        formatter.format(&lifted.instruction, &mut text);

        let bytes = instruction_bytes(object, function, lifted);
        let mut annotation = symbolic_annotation(object, lifted);
        if let Some(table) = function.jump_tables.get(&position) {
            let targets: Vec<String> = table
                .targets
                .iter()
                .map(|target| format!("{target:#x}"))
                .collect();
            annotation = format!("    ; switch -> {}", targets.join(", "));
        }
        writeln!(
            output,
            "  {:>6x}: {:<24} {text}{annotation}",
            lifted.offset, bytes,
        )
        .unwrap();
    }
}

fn instruction_bytes(
    object: &ObjectFile,
    function: &LiftedFunction,
    lifted: &LiftedInstruction,
) -> String {
    let section = object.functions[function.function].section;
    let start = lifted.offset as usize;
    let end = lifted.end_offset() as usize;
    object.sections[section].bytes[start..end]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn symbolic_annotation(object: &ObjectFile, lifted: &LiftedInstruction) -> String {
    let mut parts = Vec::new();
    if let Some(reference) = lifted.displacement {
        parts.push(format!("mem = {}", render_reference(object, reference)));
    }
    if let Some(reference) = lifted.immediate {
        parts.push(format!("imm = {}", render_reference(object, reference)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("    ; {}", parts.join(", "))
    }
}

fn render_reference(object: &ObjectFile, reference: SymbolReference) -> String {
    let name = symbol_name(object, reference.symbol);
    match reference.addend {
        0 => name,
        addend => format!("{name}{addend:+}"),
    }
}

fn symbol_name(object: &ObjectFile, index: usize) -> String {
    let symbol = &object.symbols[index];
    if symbol.name.is_empty() {
        format!("symbol#{index}")
    } else {
        symbol.name.clone()
    }
}
