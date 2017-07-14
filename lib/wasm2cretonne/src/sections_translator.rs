use translation_utils::{type_to_type, Import, TableIndex, FunctionIndex, SignatureIndex};
use cretonne::ir::{Signature, ArgumentType};
use cretonne;
use wasmparser::{Parser, ParserState, FuncType, ImportSectionEntryType, ExternalKind, WasmDecoder,
                 MemoryType, Operator};
use wasmparser;
use std::collections::HashMap;
use std::str::from_utf8;
use runtime::{WasmRuntime, Global, GlobalInit, Table, TableElementType, Memory};

pub enum SectionParsingError {
    WrongSectionContent(),
}

/// Reads the Type Section of the wasm module and returns the corresponding function signatures.
pub fn parse_function_signatures(parser: &mut Parser)
                                 -> Result<Vec<Signature>, SectionParsingError> {
    let mut signatures: Vec<Signature> = Vec::new();
    loop {
        match *parser.read() {
            ParserState::EndSection => break,
            ParserState::TypeSectionEntry(FuncType {
                                              form: wasmparser::Type::Func,
                                              ref params,
                                              ref returns,
                                          }) => {
                let mut sig = Signature::new();
                sig.argument_types
                    .extend(params
                                .iter()
                                .map(|ty| {
                        let cret_arg: cretonne::ir::Type = match type_to_type(ty) {
                            Ok(ty) => ty,
                            Err(()) => panic!("only numeric types are supported in\
                                      function signatures"),
                        };
                        ArgumentType::new(cret_arg)
                    }));
                sig.return_types
                    .extend(returns
                                .iter()
                                .map(|ty| {
                        let cret_arg: cretonne::ir::Type = match type_to_type(ty) {
                            Ok(ty) => ty,
                            Err(()) => panic!("only numeric types are supported in\
                                  function signatures"),
                        };
                        ArgumentType::new(cret_arg)
                    }));
                signatures.push(sig);
            }
            _ => return Err(SectionParsingError::WrongSectionContent()),
        }
    }
    Ok(signatures)
}

/// Retrieves the imports from the imports section of the binary.
pub fn parse_import_section(parser: &mut Parser) -> Result<Vec<Import>, SectionParsingError> {
    let mut imports = Vec::new();
    loop {
        match *parser.read() {
            ParserState::ImportSectionEntry {
                ty: ImportSectionEntryType::Function(sig), ..
            } => imports.push(Import::Function { sig_index: sig }),
            ParserState::ImportSectionEntry {
                ty: ImportSectionEntryType::Memory(MemoryType { limits: ref memlimits }), ..
            } => {
                imports.push(Import::Memory(Memory {
                                                size: memlimits.initial as usize,
                                                maximum: memlimits.maximum.map(|x| x as usize),
                                            }))
            }
            ParserState::ImportSectionEntry {
                ty: ImportSectionEntryType::Global(ref ty), ..
            } => {
                imports.push(Import::Global(Global {
                                                ty: type_to_type(&ty.content_type).unwrap(),
                                                mutability: ty.mutability != 0,
                                                initializer: GlobalInit::Import(),
                                            }));
            }
            ParserState::ImportSectionEntry {
                ty: ImportSectionEntryType::Table(ref tab), ..
            } => {
                imports.push(Import::Table(Table {
                                               ty: match type_to_type(&tab.element_type) {
                                                   Ok(t) => TableElementType::Val(t),
                                                   Err(()) => TableElementType::Func(),
                                               },
                                               size: tab.limits.initial as usize,
                                               maximum: tab.limits.maximum.map(|x| x as usize),
                                           }));
            }
            ParserState::EndSection => break,
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
    }
    Ok(imports)
}

/// Retrieves the correspondances between functions and signatures from the function section
pub fn parse_function_section(parser: &mut Parser)
                              -> Result<Vec<SignatureIndex>, SectionParsingError> {
    let mut funcs = Vec::new();
    loop {
        match *parser.read() {
            ParserState::FunctionSectionEntry(sigindex) => funcs.push(sigindex as SignatureIndex),
            ParserState::EndSection => break,
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
    }
    Ok(funcs)
}

/// Retrieves the names of the functions from the export section
pub fn parse_export_section(parser: &mut Parser)
                            -> Result<HashMap<FunctionIndex, String>, SectionParsingError> {
    let mut exports: HashMap<FunctionIndex, String> = HashMap::new();
    loop {
        match *parser.read() {
            ParserState::ExportSectionEntry {
                field,
                ref kind,
                index,
            } => {
                match kind {
                    &ExternalKind::Function => {
                        exports.insert(index as FunctionIndex,
                                       String::from(from_utf8(field).unwrap()));
                        ()
                    }
                    _ => (),//TODO: deal with other kind of exports
                }
            }
            ParserState::EndSection => break,
            ref s @ _ => {
                println!("{:?}", s);
                return Err(SectionParsingError::WrongSectionContent());
            }
        };
    }
    Ok(exports)
}

/// Retrieves the size and maximum fields of memories from the memory section
pub fn parse_memory_section(parser: &mut Parser) -> Result<Vec<Memory>, SectionParsingError> {
    let mut memories: Vec<Memory> = Vec::new();
    loop {
        match *parser.read() {
            ParserState::MemorySectionEntry(ref ty) => {
                memories.push(Memory {
                                  size: ty.limits.initial as usize,
                                  maximum: ty.limits.maximum.map(|x| x as usize),
                              })
            }
            ParserState::EndSection => break,
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
    }
    Ok(memories)
}

/// Retrieves the size and maximum fields of memories from the memory section
pub fn parse_global_section(parser: &mut Parser,
                            runtime: &mut WasmRuntime)
                            -> Result<Vec<Global>, SectionParsingError> {
    let mut globals = Vec::new();
    loop {
        let (content_type, mutability) = match *parser.read() {
            ParserState::BeginGlobalSectionEntry(ref ty) => (ty.content_type, ty.mutability),
            ParserState::EndSection => break,
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
        match *parser.read() {
            ParserState::BeginInitExpressionBody => (),
            _ => return Err(SectionParsingError::WrongSectionContent()),
        }
        let initializer = match *parser.read() {
            ParserState::InitExpressionOperator(Operator::I32Const { value }) => {
                GlobalInit::I32Const(value)
            }
            ParserState::InitExpressionOperator(Operator::I64Const { value }) => {
                GlobalInit::I64Const(value)
            }
            ParserState::InitExpressionOperator(Operator::F32Const { value }) => {
                GlobalInit::F32Const(value.bits())
            }
            ParserState::InitExpressionOperator(Operator::F64Const { value }) => {
                GlobalInit::F64Const(value.bits())
            }
            ParserState::InitExpressionOperator(Operator::GetGlobal { global_index }) => {
                GlobalInit::ImportRef(global_index as usize)
            }
            _ => return Err(SectionParsingError::WrongSectionContent()),

        };
        match *parser.read() {
            ParserState::EndInitExpressionBody => (),
            _ => return Err(SectionParsingError::WrongSectionContent()),
        }
        let global = Global {
            ty: type_to_type(&content_type).unwrap(),
            mutability: mutability != 0,
            initializer: initializer,
        };
        runtime.declare_global(global.clone());
        globals.push(global);
        match *parser.read() {
            ParserState::EndGlobalSectionEntry => (),
            _ => return Err(SectionParsingError::WrongSectionContent()),
        }
    }
    Ok(globals)
}

/// Retrieves the tables from the table section
pub fn parse_table_section(parser: &mut Parser,
                           runtime: &mut WasmRuntime)
                           -> Result<(), SectionParsingError> {
    loop {
        match *parser.read() {
            ParserState::TableSectionEntry(ref table) => {
                runtime.declare_table(Table {
                                          ty: match type_to_type(&table.element_type) {
                                              Ok(t) => TableElementType::Val(t),
                                              Err(()) => TableElementType::Func(),
                                          },
                                          size: table.limits.initial as usize,
                                          maximum: table.limits.maximum.map(|x| x as usize),
                                      })
            }
            ParserState::EndSection => break,
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
    }
    Ok(())
}

/// Retrieves the tables from the table section
pub fn parse_elements_section(parser: &mut Parser,
                              runtime: &mut WasmRuntime,
                              globals: &Vec<Global>)
                              -> Result<(), SectionParsingError> {
    loop {
        let table_index = match *parser.read() {
            ParserState::BeginElementSectionEntry(ref table_index) => *table_index as TableIndex,
            ParserState::EndSection => break,
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
        match *parser.read() {
            ParserState::BeginInitExpressionBody => (),
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
        let offset = match *parser.read() {
            ParserState::InitExpressionOperator(Operator::I32Const { value }) => value as usize,
            ParserState::InitExpressionOperator(Operator::GetGlobal { global_index }) => {
                match globals[global_index as usize].initializer {
                    GlobalInit::I32Const(val) => val as usize,
                    GlobalInit::Import() => 0, // TODO: add runtime support
                    _ => panic!("should not happen"),
                }
            }
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
        match *parser.read() {
            ParserState::EndInitExpressionBody => (),
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
        match *parser.read() {
            ParserState::ElementSectionEntryBody(ref elements) => {
                let elems: Vec<FunctionIndex> =
                    elements.iter().map(|&x| x as FunctionIndex).collect();
                runtime.declare_table_elements(table_index, offset, elems.as_slice())
            }
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
        match *parser.read() {
            ParserState::EndElementSectionEntry => (),
            _ => return Err(SectionParsingError::WrongSectionContent()),
        };
    }
    Ok(())
}
