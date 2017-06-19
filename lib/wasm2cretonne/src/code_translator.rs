use cretonne::ir::{Function, Signature, Value, Type, InstBuilder};
use cretonne::ir::types::*;
use cretonne::ir::condcodes::IntCC;
use cretonne::entity_map::EntityRef;
use cretonne::ir::frontend::{ILBuilder, FunctionBuilder};
use wasmparser::{Parser, ParserState, Operator};
use sections_translator::Import;
use std::u32;

// An opaque reference to variable.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Local(u32);
impl EntityRef for Local {
    fn new(index: usize) -> Self {
        assert!(index < (u32::MAX as usize));
        Local(index as u32)
    }

    fn index(self) -> usize {
        self.0 as usize
    }
}
impl Default for Local {
    fn default() -> Local {
        Local(u32::MAX)
    }
}

pub fn translate_function_body(parser: &mut Parser,
                               sig: Signature,
                               imports: &Option<Vec<Import>>,
                               il_builder: &mut ILBuilder<Local>)
                               -> Result<Function, String> {
    let mut func = Function::new();
    let args_num: usize = sig.argument_types.len();
    let args_types: Vec<Type> = sig.argument_types
        .iter()
        .map(|arg| arg.value_type)
        .collect();
    func.signature = sig;
    let mut value_stack: Vec<Value> = Vec::new();
    {
        let mut builder = FunctionBuilder::new(&mut func, il_builder);
        let current_ebb = builder.create_ebb();
        builder.switch_to_block(current_ebb);
        for i in 0..args_num {
            let arg_value = builder.arg_value(i as usize);
            builder.declare_var(Local(i as u32), args_types[i]);
            builder.def_var(Local(i as u32), arg_value);
        }
        loop {
            let state = parser.read();
            match *state {
                ParserState::CodeOperator(ref op) => {
                    translate_operator(op, &mut builder, imports, &mut value_stack)
                }
                ParserState::EndFunctionBody => break,
                _ => return Err(String::from("wrong content in function body")),
            }
        }
        if value_stack.len() != 0 {
            builder.ins().return_(value_stack.as_slice());
        }
    }
    Ok(func)
}

fn translate_operator(op: &Operator,
                      builder: &mut FunctionBuilder<Local>,
                      imports: &Option<Vec<Import>>,
                      stack: &mut Vec<Value>) {
    match *op {
        Operator::GetLocal { local_index } => stack.push(builder.use_var(Local(local_index))),
        Operator::I32Const { value } => stack.push(builder.ins().iconst(I32, value as i64)),
        Operator::I32Add => {
            let arg1 = stack.pop().unwrap();
            let arg2 = stack.pop().unwrap();
            stack.push(builder.ins().iadd(arg1, arg2));
        }
        Operator::I32LtS => {
            let arg1 = stack.pop().unwrap();
            let arg2 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::SignedLessThan, arg1, arg2);
            stack.push(builder.ins().sextend(I32, val));
        }
        Operator::I32LtU => {
            let arg1 = stack.pop().unwrap();
            let arg2 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::UnsignedLessThan, arg1, arg2);
            stack.push(builder.ins().uextend(I32, val));
        }
        Operator::I64Const { value } => stack.push(builder.ins().iconst(I64, value)),
        Operator::I64Add => {
            let arg1 = stack.pop().unwrap();
            let arg2 = stack.pop().unwrap();
            stack.push(builder.ins().iadd(arg1, arg2));
        }
        Operator::I64LtS => {
            let arg1 = stack.pop().unwrap();
            let arg2 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::SignedLessThan, arg1, arg2);
            stack.push(builder.ins().sextend(I32, val));
        }
        Operator::I64LtU => {
            let arg1 = stack.pop().unwrap();
            let arg2 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::UnsignedLessThan, arg1, arg2);
            stack.push(builder.ins().uextend(I32, val));
        }
        _ => unimplemented!(),
    }
}