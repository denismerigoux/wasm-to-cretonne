use translation_utils::{type_to_type, Import, TableIndex, FunctionIndex, SignatureIndex,
                        MemoryIndex};
use cretonne::ir::{Signature, ArgumentType};
use cretonne;
use wasmparser::{Parser, ParserState, FuncType, ImportSectionEntryType, ExternalKind, WasmDecoder,
                 MemoryType, Operator};
use wasmparser;
use std::collections::HashMap;
use std::str::from_utf8;
use runtime::{WasmRuntime, Global, GlobalInit, Table, TableElementType, Memory};

pub enum SectionParsingError {
    WrongSectionContent(String),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        match *parser.read() {
            ParserState::BeginInitExpressionBody => (),
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),

        };
        match *parser.read() {
            ParserState::EndInitExpressionBody => (),
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        }
    }
    Ok(globals)
}

pub fn parse_data_section(parser: &mut Parser,
                          runtime: &mut WasmRuntime,
                          globals: &Vec<Global>)
                          -> Result<(), SectionParsingError> {
    loop {
        let memory_index = match *parser.read() {
            ParserState::BeginDataSectionEntry(memory_index) => memory_index,
            ParserState::EndDataSectionEntry => break,
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        match *parser.read() {
            ParserState::BeginInitExpressionBody => (),
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        let offset = match *parser.read() {
            ParserState::InitExpressionOperator(Operator::I32Const { value }) => {
                if value < 0 {
                    return Err(SectionParsingError::WrongSectionContent(String::from("negative offset value",),),);
                } else {
                    value as usize
                }
            }
            ParserState::InitExpressionOperator(Operator::GetGlobal { global_index }) => {
                match globals[global_index as usize].initializer {
                    GlobalInit::I32Const(value) => {
                        if value < 0 {
                            return Err(SectionParsingError::WrongSectionContent(String::from("negative offset value",),),);
                        } else {
                            value as usize
                        }
                    }
                    GlobalInit::Import() => {
                        return Err(SectionParsingError::WrongSectionContent(String::from("imported globals not supported",),),)
                    } // TODO: add runtime support
                    _ => panic!("should not happen"),
                }
            }
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        match *parser.read() {
            ParserState::EndInitExpressionBody => (),
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        let data = match *parser.read() {
            ParserState::DataSectionEntryBody(data) => data,
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        match runtime.declare_data_initialization(memory_index as MemoryIndex, offset, data) {
            Ok(()) => (),
            Err(s) => return Err(SectionParsingError::WrongSectionContent(format!("{}", s))),
        };
    }
    Ok(())
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
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
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        match *parser.read() {
            ParserState::BeginInitExpressionBody => (),
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        let offset = match *parser.read() {
            ParserState::InitExpressionOperator(Operator::I32Const { value }) => {
                if value < 0 {
                    return Err(SectionParsingError::WrongSectionContent(String::from("negative offset value",),),);
                } else {
                    value as usize
                }
            }
            ParserState::InitExpressionOperator(Operator::GetGlobal { global_index }) => {
                match globals[global_index as usize].initializer {
                    GlobalInit::I32Const(value) => {
                        if value < 0 {
                            return Err(SectionParsingError::WrongSectionContent(String::from("negative offset value",),),);
                        } else {
                            value as usize
                        }
                    }
                    GlobalInit::Import() => 0, // TODO: add runtime support
                    _ => panic!("should not happen"),
                }
            }
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        match *parser.read() {
            ParserState::EndInitExpressionBody => (),
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        match *parser.read() {
            ParserState::ElementSectionEntryBody(ref elements) => {
                let elems: Vec<FunctionIndex> =
                    elements.iter().map(|&x| x as FunctionIndex).collect();
                runtime.declare_table_elements(table_index, offset, elems.as_slice())
            }
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
        match *parser.read() {
            ParserState::EndElementSectionEntry => (),
            ref s @ _ => return Err(SectionParsingError::WrongSectionContent(format!("{:?}", s))),
        };
    }
    Ok(())
}
