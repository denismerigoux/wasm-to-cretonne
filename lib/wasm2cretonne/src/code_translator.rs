//! This module contains the bulk of the interesting code performing the translation between
//! WebAssembly and Cretonne IL.
//!
//! The translation is done in one pass, opcode by opcode. Two main data structures are used during
//! code translations: the value stack and the control stack. The value stack mimics the execution
//! of the WebAssembly stack machine: each instruction result is pushed onto the stack and
//! instruction arguments are popped off the stack. Similarly, when encountering a control flow
//! block, it is pushed onto the control stack and popped off when encountering the corresponding
//! `End`.
//!
//! Another data structure, the translation state, records information concerning unreachable code
//! status and about if inserting a return at the end of the function is necessary.
//!
//! Some of the WebAssembly instructions need information about the runtime to be translated:
//!
//! - the loads and stores need the memory base address;
//! - the `get_global` et `set_global` instructions depends on how the globals are implemented;
//! - `current_memory` and `grow_memory` are runtime functions;
//! - `call_indirect` has to translate the function index into the address of where this
//!    is;
//!
//! That is why `translate_function_body` takes an object having the `WasmRuntime` trait as
//! argument.
use cretonne::ir::{Function, Signature, Value, Type, InstBuilder, FunctionName, Ebb, FuncRef,
                   SigRef, ExtFuncData, Inst, MemFlags};
use cretonne::ir::types::*;
use cretonne::ir::immediates::{Ieee32, Ieee64, Offset32};
use cretonne::ir::condcodes::{IntCC, FloatCC};
use cton_frontend::{ILBuilder, FunctionBuilder};
use wasmparser::{Parser, ParserState, Operator, WasmDecoder, MemoryImmediate};
use translation_utils::{f32_translation, f64_translation, type_to_type, translate_type, Local,
                        GlobalIndex, FunctionIndex, SignatureIndex};
use std::collections::{HashMap, HashSet};
use runtime::WasmRuntime;
use std::u32;


/// A control stack frame can be an `if`, a `block` or a `loop`, each one having the following
/// fields:
///
/// - `destination`: reference to the `Ebb` that will hold the code after the control block;
/// - `return_values`: types of the values returned by the control block;
/// - `original_stack_size`: size of the value stack at the beginning of the control block.
///
/// Moreover, the `if` frame has the `branch_inst` field that points to the `brz` instruction
/// separating the `true` and `false` branch. The `loop` frame has a `header` field that references
/// the `Ebb` that contains the beginning of the body of the loop.
#[derive(Debug)]
enum ControlStackFrame {
    If {
        destination: Ebb,
        branch_inst: Inst,
        return_values: Vec<Type>,
        original_stack_size: usize,
    },
    Block {
        destination: Ebb,
        return_values: Vec<Type>,
        original_stack_size: usize,
    },
    Loop {
        destination: Ebb,
        header: Ebb,
        return_values: Vec<Type>,
        original_stack_size: usize,
    },
}

/// Helper methods for the control stack objects.
impl ControlStackFrame {
    fn return_values(&self) -> &[Type] {
        match self {
            &ControlStackFrame::If { ref return_values, .. } |
            &ControlStackFrame::Block { ref return_values, .. } |
            &ControlStackFrame::Loop { ref return_values, .. } => return_values.as_slice(),
        }
    }
    fn following_code(&self) -> Ebb {
        match self {
            &ControlStackFrame::If { destination, .. } |
            &ControlStackFrame::Block { destination, .. } |
            &ControlStackFrame::Loop { destination, .. } => destination,
        }
    }
    fn br_destination(&self) -> Ebb {
        match self {
            &ControlStackFrame::If { destination, .. } |
            &ControlStackFrame::Block { destination, .. } => destination,
            &ControlStackFrame::Loop { header, .. } => header,
        }
    }
    fn original_stack_size(&self) -> usize {
        match self {
            &ControlStackFrame::If { original_stack_size, .. } |
            &ControlStackFrame::Block { original_stack_size, .. } |
            &ControlStackFrame::Loop { original_stack_size, .. } => original_stack_size,
        }
    }
    fn is_loop(&self) -> bool {
        match self {
            &ControlStackFrame::If { .. } |
            &ControlStackFrame::Block { .. } => false,
            &ControlStackFrame::Loop { .. } => true,
        }
    }
}

/// Contains information passed along during the translation and that records:
///
/// - if the last instruction added was a `return`;
/// - the depth of the two unreachable control blocks stacks, that are manipulated when translating
///   unreachable code;
/// - all the `Ebb`s referenced by `br_table` instructions, because those are always reachable even
///   if they are at a point of the code that would have been unreachable otherwise.
struct TranslationState {
    last_inst_return: bool,
    phantom_unreachable_stack_depth: usize,
    real_unreachable_stack_depth: usize,
    br_table_reachable_ebbs: HashSet<Ebb>,
}

/// Holds mappings between the function and signatures indexes in the Wasm module and their
/// references as imports of the Cretonne IL function.
#[derive(Clone,Debug)]
pub struct FunctionImports {
    /// Mappings index in function index space -> index in function local imports
    pub functions: HashMap<FunctionIndex, FuncRef>,
    /// Mappings index in signature index space -> index in signature local imports
    pub signatures: HashMap<SignatureIndex, SigRef>,
}

impl FunctionImports {
    fn new() -> FunctionImports {
        FunctionImports {
            functions: HashMap::new(),
            signatures: HashMap::new(),
        }
    }
}

/// Returns a well-formed Cretonne IL function from a wasm function body and a signature.
pub fn translate_function_body(parser: &mut Parser,
                               function_index: FunctionIndex,
                               sig: Signature,
                               locals: &Vec<(usize, Type)>,
                               exports: &Option<HashMap<FunctionIndex, String>>,
                               signatures: &Vec<Signature>,
                               functions: &Vec<SignatureIndex>,
                               il_builder: &mut ILBuilder<Local>,
                               runtime: &mut WasmRuntime)
                               -> Result<(Function, FunctionImports), String> {
    runtime.next_function();
    // First we build the Function object with its name and signature
    let mut func = Function::new();
    let args_num: usize = sig.argument_types.len();
    let args_types: Vec<Type> = sig.argument_types
        .iter()
        .map(|arg| arg.value_type)
        .collect();
    func.signature = sig.clone();
    match exports {
        &None => (),
        &Some(ref exports) => {
            match exports.get(&function_index) {
                None => (),
                Some(name) => func.name = FunctionName::new(name.clone()),
            }
        }
    }
    let mut func_imports = FunctionImports::new();
    let mut stack: Vec<Value> = Vec::new();
    let mut control_stack: Vec<ControlStackFrame> = Vec::new();
    /// We introduce a arbitrary scope for the FunctionBuilder object
    {
        let mut builder = FunctionBuilder::new(&mut func, il_builder);
        let first_ebb = builder.create_ebb();
        builder.switch_to_block(first_ebb, &[]);
        builder.seal_block(first_ebb);
        for i in 0..args_num {
            // First we declare the function arguments' as non-SSA vars because they will be
            // accessed by get_local
            let arg_value = builder.arg_value(i as usize);
            builder.declare_var(Local(i as u32), args_types[i]);
            builder.def_var(Local(i as u32), arg_value);
        }
        // We also declare and initialize to 0 the local variables
        let mut local_index = args_num;
        for &(loc_count, ty) in locals {
            let val = match ty {
                I32 => builder.ins().iconst(ty, 0),
                I64 => builder.ins().iconst(ty, 0),
                F32 => builder.ins().f32const(Ieee32::with_bits(0)),
                F64 => builder.ins().f64const(Ieee64::with_bits(0)),
                _ => panic!("should not happen"),
            };
            for _ in 0..loc_count {
                builder.declare_var(Local(local_index as u32), ty);
                builder.def_var(Local(local_index as u32), val);
                local_index += 1;
            }
        }
        let mut state = TranslationState {
            last_inst_return: false,
            phantom_unreachable_stack_depth: 0,
            real_unreachable_stack_depth: 0,
            br_table_reachable_ebbs: HashSet::new(),
        };
        // We initialize the control stack with the implicit function block
        let end_ebb = builder.create_ebb();
        control_stack.push(ControlStackFrame::Block {
                               destination: end_ebb,
                               original_stack_size: 0,
                               return_values: sig.return_types
                                   .iter()
                                   .map(|argty| argty.value_type)
                                   .collect(),
                           });
        // Now the main loop that reads every wasm instruction and translates it
        loop {
            let parser_state = parser.read();
            match *parser_state {
                ParserState::CodeOperator(ref op) => {
                    if state.phantom_unreachable_stack_depth +
                       state.real_unreachable_stack_depth > 0 {
                        translate_unreachable_operator(op,
                                                       &mut builder,
                                                       &mut stack,
                                                       &mut control_stack,
                                                       &mut state)
                    } else {
                        translate_operator(op,
                                           &mut builder,
                                           runtime,
                                           &mut stack,
                                           &mut control_stack,
                                           &mut state,
                                           &sig,
                                           &functions,
                                           &signatures,
                                           &exports,
                                           &mut func_imports)
                    }
                }

                ParserState::EndFunctionBody => break,
                _ => return Err(String::from("wrong content in function body")),
            }
        }
        // In WebAssembly, the final return instruction is implicit so we need to build it
        // explicitely in Cretonne IL.
        if !state.last_inst_return && !builder.is_filled() &&
           (!builder.is_unreachable() || !builder.is_pristine()) {
            let cut_index = stack.len() - sig.return_types.len();
            let return_vals = stack.split_off(cut_index);
            builder.ins().return_(return_vals.as_slice());
        }
        // Because the function has an implicit block as body, we need to explicitely close it.
        let frame = control_stack.pop().unwrap();
        builder.switch_to_block(frame.following_code(), frame.return_values());
        builder.seal_block(frame.following_code());
        // If the block is reachable we also have to include a return instruction in it.
        if !builder.is_unreachable() {
            stack.truncate(frame.original_stack_size());
            stack.extend_from_slice(builder.ebb_args(frame.following_code()));
            let cut_index = stack.len() - sig.return_types.len();
            let return_vals = stack.split_off(cut_index);
            builder.ins().return_(return_vals.as_slice());
        }
    }
    Ok((func, func_imports))
}

/// Translates wasm operators into Cretonne IL instructions. Returns `true` if it inserted
/// a return.
fn translate_operator(op: &Operator,
                      builder: &mut FunctionBuilder<Local>,
                      runtime: &mut WasmRuntime,
                      stack: &mut Vec<Value>,
                      control_stack: &mut Vec<ControlStackFrame>,
                      state: &mut TranslationState,
                      sig: &Signature,
                      functions: &Vec<SignatureIndex>,
                      signatures: &Vec<Signature>,
                      exports: &Option<HashMap<FunctionIndex, String>>,
                      func_imports: &mut FunctionImports) {
    state.last_inst_return = false;
    // This big match treats all Wasm code operators.
    match *op {
        /********************************** Locals ****************************************
         *  `get_local` and `set_local` are treated as non-SSA variables and will completely
         *  diseappear in the Cretonne Code
         ***********************************************************************************/
        Operator::GetLocal { local_index } => stack.push(builder.use_var(Local(local_index))),
        Operator::SetLocal { local_index } => {
            let val = stack.pop().unwrap();
            builder.def_var(Local(local_index), val);
        }
        Operator::TeeLocal { local_index } => {
            let val = stack.last().unwrap();
            builder.def_var(Local(local_index), *val);
        }
        /********************************** Globals ****************************************
         *  `get_global` and `set_global` are handled by the runtime.
         ***********************************************************************************/
        Operator::GetGlobal { global_index } => {
            let val = runtime.translate_get_global(builder, global_index as GlobalIndex);
            stack.push(val);
        }
        Operator::SetGlobal { global_index } => {
            let val = stack.pop().unwrap();
            runtime.translate_set_global(builder, global_index as GlobalIndex, val);
        }
        /********************************* Stack misc ***************************************
         *  `drop`, `nop`,  `unreachable` and `select`.
         ***********************************************************************************/
        Operator::Drop => {
            stack.pop();
        }
        Operator::Select => {
            let cond = stack.pop().unwrap();
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().select(cond, arg2, arg1));
        }
        Operator::Nop => {
            // We do nothing
        }
        Operator::Unreachable => {
            builder.ins().trap();
            state.real_unreachable_stack_depth = 1;
        }
        /***************************** Control flow blocks **********************************
         *  When starting a control flow block, we create a new `Ebb` that will hold the code
         *  after the block, and we push a frame on the control stack. Depending on the type
         *  of block, we create a new `Ebb` for the body of the block with an associated
         *  jump instruction.
         *
         *  The `End` instruction pops the last control frame from the control stack, seals
         *  the destination block (since `br` instructions targeting it only appear inside the
         *  block and have already been translated) and modify the value stack to use the
         *  possible `Ebb`'s arguments values.
         ***********************************************************************************/
        Operator::Block { ty } => {
            let next = builder.create_ebb();
            match type_to_type(&ty) {
                Ok(ty_cre) => {
                    builder.append_ebb_arg(next, ty_cre);
                }
                Err(_) => {}
            }
            control_stack.push(ControlStackFrame::Block {
                                   destination: next,
                                   return_values: translate_type(ty).unwrap(),
                                   original_stack_size: stack.len(),
                               });
        }
        Operator::Loop { ty } => {
            let loop_body = builder.create_ebb();
            let next = builder.create_ebb();
            match type_to_type(&ty) {
                Ok(ty_cre) => {
                    builder.append_ebb_arg(next, ty_cre);
                }
                Err(_) => {}
            }
            builder.ins().jump(loop_body, &[]);
            control_stack.push(ControlStackFrame::Loop {
                                   destination: next,
                                   header: loop_body,
                                   return_values: translate_type(ty).unwrap(),
                                   original_stack_size: stack.len(),
                               });
            builder.switch_to_block(loop_body, &[]);
        }
        Operator::If { ty } => {
            let val = stack.pop().unwrap();
            let if_not = builder.create_ebb();
            let jump_inst = builder.ins().brz(val, if_not, &[]);
            // Here we append an argument to an Ebb targeted by an argumentless jump instruction
            // But in fact there are two cases:
            // - either the If does not have a Else clause, in that case ty = EmptyBlock
            //   and we add nothing;
            // - either the If have an Else clause, in that case the destination of this jump
            //   instruction will be changed later when we translate the Else operator.
            match type_to_type(&ty) {
                Ok(ty_cre) => {
                    builder.append_ebb_arg(if_not, ty_cre);
                }
                Err(_) => {}
            }
            control_stack.push(ControlStackFrame::If {
                                   destination: if_not,
                                   branch_inst: jump_inst,
                                   return_values: translate_type(ty).unwrap(),
                                   original_stack_size: stack.len(),
                               });
        }
        Operator::Else => {
            // We take the control frame pushed by the if, use its ebb as the else body
            // and push a new control frame with a new ebb for the code after the if/then/else
            // At the end of the then clause we jump to the destination
            let (destination, return_values, branch_inst) = match &control_stack[control_stack.len() -
                                                                   1] {
                &ControlStackFrame::If {
                    destination,
                    ref return_values,
                    branch_inst,
                    ..
                } => (destination, return_values, branch_inst),
                _ => panic!("should not happen"),
            };
            let cut_index = stack.len() - return_values.len();
            let jump_args = stack.split_off(cut_index);
            builder.ins().jump(destination, jump_args.as_slice());
            // We change the target of the branch instruction
            let else_ebb = builder.create_ebb();
            builder.change_jump_destination(branch_inst, else_ebb);
            builder.seal_block(else_ebb);
            builder.switch_to_block(else_ebb, &[]);
        }
        Operator::End => {
            let frame = control_stack.pop().unwrap();
            if !builder.is_unreachable() || !builder.is_pristine() {
                let cut_index = stack.len() - frame.return_values().len();
                let jump_args = stack.split_off(cut_index);
                builder
                    .ins()
                    .jump(frame.following_code(), jump_args.as_slice());
            }
            builder.switch_to_block(frame.following_code(), frame.return_values());
            builder.seal_block(frame.following_code());
            // If it is a loop we also have to seal the body loop block
            match frame {
                ControlStackFrame::Loop { header, .. } => builder.seal_block(header),
                _ => {}
            }
            stack.truncate(frame.original_stack_size());
            stack.extend_from_slice(builder.ebb_args(frame.following_code()));
        }
        /**************************** Branch instructions *********************************
         * The branch instructions all have as arguments a target nesting level, which
         * corresponds to how many control stack frames do we have to pop to get the
         * destination `Ebb`.
         *
         * Once the destination `Ebb` is found, we sometimes have to declare a certain depth
         * of the stack unreachable, because some branch instructions are terminator.
         *
         * The `br_table` case is much more complicated because Cretonne's `br_table` instruction
         * does not support jump arguments like all the other branch instructions. That is why, in
         * the case where we would use jump arguments for every other branch instructions, we
         * need to split the critical edges leaving the `br_tables` by creating one `Ebb` per
         * table destination; the `br_table` will point to these newly created `Ebbs` and these
         * `Ebb`s contain only a jump instruction pointing to the final destination, this time with
         * jump arguments.
         *
         * This system is also implemented in Cretonne's SSA construction algorithm, because
         * `use_var` located in a destination `Ebb` of a `br_table` might trigger the addition
         * of jump arguments in each predecessor branch instruction, one of which might be a
         * `br_table`.
         ***********************************************************************************/
        Operator::Br { relative_depth } => {
            let frame = &control_stack[control_stack.len() - 1 - (relative_depth as usize)];
            let jump_args = if frame.is_loop() {
                Vec::new()
            } else {
                let cut_index = stack.len() - frame.return_values().len();
                stack.split_off(cut_index)
            };
            builder
                .ins()
                .jump(frame.br_destination(), jump_args.as_slice());
            // We signal that all the code that follows until the next End is unreachable
            state.real_unreachable_stack_depth = 1 + relative_depth as usize;
        }
        Operator::BrIf { relative_depth } => {
            let val = stack.pop().unwrap();
            let frame = &control_stack[control_stack.len() - 1 - (relative_depth as usize)];
            let cut_index = stack.len() - frame.return_values().len();
            let jump_args = stack.split_off(cut_index);
            builder
                .ins()
                .brnz(val, frame.br_destination(), jump_args.as_slice());
            // The values returned by the branch are still available for the reachable
            // code that comes after it
            stack.extend(jump_args);
        }
        Operator::BrTable { ref table } => {
            let (depths, default) = table.read_table();
            let mut min_depth = default;
            for depth in depths.iter() {
                if *depth < min_depth {
                    min_depth = *depth;
                }
            }
            let jump_args_count = control_stack[control_stack.len() - 1 - (min_depth as usize)]
                .return_values()
                .len();
            if jump_args_count == 0 {
                // No jump arguments
                let val = stack.pop().unwrap();
                if depths.len() > 0 {
                    let jt = builder.create_jump_table();
                    for (index, depth) in depths.iter().enumerate() {
                        let ebb = control_stack[control_stack.len() - 1 - (*depth as usize)]
                            .br_destination();
                        builder.insert_jump_table_entry(jt, index, ebb);
                        state.br_table_reachable_ebbs.insert(ebb);
                    }
                    builder.ins().br_table(val, jt);
                }
                let ebb = control_stack[control_stack.len() - 1 - (default as usize)]
                    .br_destination();
                builder.ins().jump(ebb, &[]);
                state.real_unreachable_stack_depth = 1 + min_depth as usize;
            } else {
                // Here we have jump arguments, but Cretonne's br_table doesn't support them
                // We then proceed to split the edges going out of the br_table
                let val = stack.pop().unwrap();
                let cut_index = stack.len() - jump_args_count;
                let jump_args = stack.split_off(cut_index);
                if depths.len() > 0 {
                    let jt = builder.create_jump_table();
                    let dest_ebbs: HashMap<usize, Ebb> = depths
                        .iter()
                        .enumerate()
                        .fold(HashMap::new(), |mut acc, (index, &depth)| {
                            if acc.get(&(depth as usize)).is_none() {
                                let branch_ebb = builder.create_ebb();
                                builder.insert_jump_table_entry(jt, index, branch_ebb);
                                acc.insert(depth as usize, branch_ebb);
                                return acc;
                            };
                            let branch_ebb = acc.get(&(depth as usize)).unwrap().clone();
                            builder.insert_jump_table_entry(jt, index, branch_ebb);
                            acc
                        });
                    builder.ins().br_table(val, jt);
                    let default_ebb = control_stack[control_stack.len() - 1 - (default as usize)]
                        .br_destination();
                    builder.ins().jump(default_ebb, jump_args.as_slice());
                    stack.extend(jump_args.clone());
                    for (depth, dest_ebb) in dest_ebbs {
                        builder.switch_to_block(dest_ebb, &[]);
                        builder.seal_block(dest_ebb);
                        let real_dest_ebb = control_stack[control_stack.len() - 1 -
                        (depth as usize)]
                                .br_destination();
                        builder.ins().jump(real_dest_ebb, jump_args.as_slice());
                        state.br_table_reachable_ebbs.insert(dest_ebb);
                    }
                    state.real_unreachable_stack_depth = 1 + min_depth as usize;
                } else {
                    let ebb = control_stack[control_stack.len() - 1 - (default as usize)]
                        .br_destination();
                    builder.ins().jump(ebb, jump_args.as_slice());
                    stack.extend(jump_args);
                    state.real_unreachable_stack_depth = 1 + min_depth as usize;
                }
            }
        }
        Operator::Return => {
            let return_count = sig.return_types.len();
            let cut_index = stack.len() - return_count;
            let return_args = stack.split_off(cut_index);
            builder.ins().return_(return_args.as_slice());
            state.last_inst_return = true;
            state.real_unreachable_stack_depth = 1;
        }
        /************************************ Calls ****************************************
         * The call instructions pop off their arguments from the stack and append their
         * return values to it. `call_indirect` needs runtime support because there is an
         * argument referring to an index in the external functions table of the module.
         ************************************************************************************/
        Operator::Call { function_index } => {
            let args_num = args_count(function_index as usize, functions, signatures);
            let cut_index = stack.len() - args_num;
            let call_args = stack.split_off(cut_index);
            let internal_function_index = find_function_import(function_index as usize,
                                                               builder,
                                                               func_imports,
                                                               functions,
                                                               exports,
                                                               signatures);
            let call_inst = builder
                .ins()
                .call(internal_function_index, call_args.as_slice());
            let ret_values = builder.inst_results(call_inst);
            for val in ret_values {
                stack.push(*val);
            }
        }
        Operator::CallIndirect {
            index,
            table_index: _,
        } => {
            // index is the index of the function's signature and table_index is the index
            // of the table to search the function in
            // TODO: have runtime support for tables
            let sigref = find_signature_import(index as usize, builder, func_imports, signatures);
            let args_num = builder.signature(sigref).unwrap().argument_types.len();
            let index_val = stack.pop().unwrap();
            let cut_index = stack.len() - args_num;
            let call_args = stack.split_off(cut_index);
            let ret_values =
                runtime.translate_call_indirect(builder, sigref, index_val, call_args.as_slice());
            for val in ret_values {
                stack.push(*val);
            }
        }
        /******************************* Memory management ***********************************
         * Memory management is handled by runtime. It is usually translated into calls to
         * special functions.
         ************************************************************************************/
        Operator::GrowMemory { reserved: _ } => {
            let val = stack.pop().unwrap();
            stack.push(runtime.translate_grow_memory(builder, val));
        }
        Operator::CurrentMemory { reserved: _ } => {
            stack.push(runtime.translate_current_memory(builder));
        }
        /******************************* Load instructions ***********************************
         * Wasm specifies an integer alignment flag but we drop it in Cretonne.
         * The memory base address is provided by the runtime.
         * TODO: differentiate between 32 bit and 64 bit architecture, to put the uextend or not
         ************************************************************************************/
        Operator::I32Load8U { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().uload8(I32, memflags, addr, memoffset))
        }
        Operator::I32Load16U { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().uload8(I32, memflags, addr, memoffset))
        }
        Operator::I32Load8S { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().sload8(I32, memflags, addr, memoffset))
        }
        Operator::I32Load16S { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().sload8(I32, memflags, addr, memoffset))
        }
        Operator::I64Load8U { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().uload8(I64, memflags, addr, memoffset))
        }
        Operator::I64Load16U { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().uload16(I64, memflags, addr, memoffset))
        }
        Operator::I64Load8S { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().sload8(I64, memflags, addr, memoffset))
        }
        Operator::I64Load16S { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().sload16(I64, memflags, addr, memoffset))
        }
        Operator::I64Load32S { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().sload32(memflags, addr, memoffset))
        }
        Operator::I64Load32U { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().uload32(memflags, addr, memoffset))
        }
        Operator::I32Load { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().load(I32, memflags, addr, memoffset))
        }
        Operator::F32Load { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().load(F32, memflags, addr, memoffset))
        }
        Operator::I64Load { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().load(I64, memflags, addr, memoffset))
        }
        Operator::F64Load { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            stack.push(builder.ins().load(F64, memflags, addr, memoffset))
        }
        /****************************** Store instructions ***********************************
         * Wasm specifies an integer alignment flag but we drop it in Cretonne.
         * The memory base address is provided by the runtime.
         * TODO: differentiate between 32 bit and 64 bit architecture, to put the uextend or not
         ************************************************************************************/
        Operator::I32Store { memory_immediate: MemoryImmediate { flags: _, offset } } |
        Operator::I64Store { memory_immediate: MemoryImmediate { flags: _, offset } } |
        Operator::F32Store { memory_immediate: MemoryImmediate { flags: _, offset } } |
        Operator::F64Store { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let val = stack.pop().unwrap();
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            builder.ins().store(memflags, val, addr, memoffset);
        }
        Operator::I32Store8 { memory_immediate: MemoryImmediate { flags: _, offset } } |
        Operator::I64Store8 { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let val = stack.pop().unwrap();
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            builder.ins().istore8(memflags, val, addr, memoffset);
        }
        Operator::I32Store16 { memory_immediate: MemoryImmediate { flags: _, offset } } |
        Operator::I64Store16 { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let val = stack.pop().unwrap();
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            builder.ins().istore16(memflags, val, addr, memoffset);
        }
        Operator::I64Store32 { memory_immediate: MemoryImmediate { flags: _, offset } } => {
            let val = stack.pop().unwrap();
            let address_i32 = stack.pop().unwrap();
            let base = runtime.translate_memory_base_adress(builder, 0);
            let address_i64 = builder.ins().uextend(I64, address_i32);
            let addr = builder.ins().iadd(base, address_i64);
            let memflags = MemFlags::new();
            let memoffset = Offset32::new(offset as i32);
            builder.ins().istore32(memflags, val, addr, memoffset);
        }
        /****************************** Nullary Operators ************************************/
        Operator::I32Const { value } => stack.push(builder.ins().iconst(I32, value as i64)),
        Operator::I64Const { value } => stack.push(builder.ins().iconst(I64, value)),
        Operator::F32Const { value } => {
            stack.push(builder.ins().f32const(f32_translation(value)));
        }
        Operator::F64Const { value } => {
            stack.push(builder.ins().f64const(f64_translation(value)));
        }
        /******************************* Unary Operators *************************************/
        Operator::I32Clz => {
            let arg = stack.pop().unwrap();
            let val = builder.ins().clz(arg);
            stack.push(builder.ins().sextend(I32, val));
        }
        Operator::I64Clz => {
            let arg = stack.pop().unwrap();
            let val = builder.ins().clz(arg);
            stack.push(builder.ins().sextend(I64, val));
        }
        Operator::I32Ctz => {
            let val = stack.pop().unwrap();
            let short_res = builder.ins().ctz(val);
            stack.push(builder.ins().sextend(I32, short_res));
        }
        Operator::I64Ctz => {
            let val = stack.pop().unwrap();
            let short_res = builder.ins().ctz(val);
            stack.push(builder.ins().sextend(I64, short_res));
        }
        Operator::I32Popcnt => {
            let arg = stack.pop().unwrap();
            let val = builder.ins().popcnt(arg);
            stack.push(builder.ins().sextend(I32, val));
        }
        Operator::I64Popcnt => {
            let arg = stack.pop().unwrap();
            let val = builder.ins().popcnt(arg);
            stack.push(builder.ins().sextend(I64, val));
        }
        Operator::I64ExtendSI32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().sextend(I64, val));
        }
        Operator::I64ExtendUI32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().uextend(I64, val));
        }
        Operator::I32WrapI64 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().ireduce(I32, val));
        }
        Operator::F32Sqrt |
        Operator::F64Sqrt => {
            let arg = stack.pop().unwrap();
            stack.push(builder.ins().sqrt(arg));
        }
        Operator::F32Ceil |
        Operator::F64Ceil => {
            let arg = stack.pop().unwrap();
            stack.push(builder.ins().ceil(arg));
        }
        Operator::F32Floor |
        Operator::F64Floor => {
            let arg = stack.pop().unwrap();
            stack.push(builder.ins().floor(arg));
        }
        Operator::F32Trunc |
        Operator::F64Trunc => {
            let arg = stack.pop().unwrap();
            stack.push(builder.ins().trunc(arg));
        }
        Operator::F32Nearest |
        Operator::F64Nearest => {
            let arg = stack.pop().unwrap();
            stack.push(builder.ins().nearest(arg));
        }
        Operator::F32Abs | Operator::F64Abs => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fabs(val));
        }
        Operator::F32Neg | Operator::F64Neg => {
            let arg = stack.pop().unwrap();
            stack.push(builder.ins().fneg(arg));
        }
        Operator::F64ConvertUI64 |
        Operator::F64ConvertUI32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fcvt_from_uint(F64, val));
        }
        Operator::F64ConvertSI64 |
        Operator::F64ConvertSI32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fcvt_from_sint(F64, val));
        }
        Operator::F32ConvertSI64 |
        Operator::F32ConvertSI32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fcvt_from_sint(F32, val));
        }
        Operator::F32ConvertUI64 |
        Operator::F32ConvertUI32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fcvt_from_uint(F32, val));
        }
        Operator::F64PromoteF32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fpromote(F64, val));
        }
        Operator::F32DemoteF64 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fdemote(F32, val));
        }
        Operator::I64TruncSF64 |
        Operator::I64TruncSF32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fcvt_to_sint(I64, val));
        }
        Operator::I32TruncSF64 |
        Operator::I32TruncSF32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fcvt_to_sint(I32, val));
        }
        Operator::I64TruncUF64 |
        Operator::I64TruncUF32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fcvt_to_uint(I64, val));
        }
        Operator::I32TruncUF64 |
        Operator::I32TruncUF32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().fcvt_to_uint(I32, val));
        }
        Operator::F32ReinterpretI32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().bitcast(F32, val));
        }
        Operator::F64ReinterpretI64 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().bitcast(F64, val));
        }
        Operator::I32ReinterpretF32 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().bitcast(I32, val));
        }
        Operator::I64ReinterpretF64 => {
            let val = stack.pop().unwrap();
            stack.push(builder.ins().bitcast(I64, val));
        }
        /****************************** Binary Operators ************************************/
        Operator::I32Add | Operator::I64Add => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().iadd(arg1, arg2));
        }
        Operator::I32And | Operator::I64And => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().band(arg1, arg2));
        }
        Operator::I32Or | Operator::I64Or => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().bor(arg1, arg2));
        }
        Operator::I32Xor | Operator::I64Xor => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().bxor(arg1, arg2));
        }
        Operator::I32Shl | Operator::I64Shl => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().ishl(arg1, arg2));
        }
        Operator::I32ShrS |
        Operator::I64ShrS => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().sshr(arg1, arg2));
        }
        Operator::I32ShrU |
        Operator::I64ShrU => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().ushr(arg1, arg2));
        }
        Operator::I32Rotl |
        Operator::I64Rotl => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().rotl(arg1, arg2));
        }
        Operator::I32Rotr |
        Operator::I64Rotr => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().rotr(arg1, arg2));
        }
        Operator::F32Add | Operator::F64Add => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().fadd(arg1, arg2));
        }
        Operator::I32Sub | Operator::I64Sub => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().isub(arg1, arg2));
        }
        Operator::F32Sub | Operator::F64Sub => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().fsub(arg1, arg2));
        }
        Operator::I32Mul | Operator::I64Mul => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().imul(arg1, arg2));
        }
        Operator::F32Mul | Operator::F64Mul => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().fmul(arg1, arg2));
        }
        Operator::F32Div | Operator::F64Div => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().fdiv(arg1, arg2));
        }
        Operator::I32DivS |
        Operator::I64DivS => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().sdiv(arg1, arg2));
        }
        Operator::I32DivU |
        Operator::I64DivU => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().udiv(arg1, arg2));
        }
        Operator::I32RemS |
        Operator::I64RemS => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().srem(arg1, arg2));
        }
        Operator::I32RemU |
        Operator::I64RemU => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().urem(arg1, arg2));
        }
        Operator::F32Min | Operator::F64Min => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().fmin(arg1, arg2));
        }
        Operator::F32Max | Operator::F64Max => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().fmax(arg1, arg2));
        }
        Operator::F32Copysign |
        Operator::F64Copysign => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            stack.push(builder.ins().fcopysign(arg1, arg2));
        }
        /**************************** Comparison Operators **********************************/
        Operator::I32LtS | Operator::I64LtS => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::SignedLessThan, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32LtU | Operator::I64LtU => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::UnsignedLessThan, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32LeS | Operator::I64LeS => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::SignedLessThanOrEqual, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32LeU | Operator::I64LeU => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder
                .ins()
                .icmp(IntCC::UnsignedLessThanOrEqual, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32GtS | Operator::I64GtS => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::SignedGreaterThan, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32GtU | Operator::I64GtU => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::UnsignedGreaterThan, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32GeS | Operator::I64GeS => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder
                .ins()
                .icmp(IntCC::SignedGreaterThanOrEqual, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32GeU | Operator::I64GeU => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32Eqz | Operator::I64Eqz => {
            let arg = stack.pop().unwrap();
            let val = builder.ins().icmp_imm(IntCC::Equal, arg, 0);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32Eq | Operator::I64Eq => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::Equal, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::F32Eq | Operator::F64Eq => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().fcmp(FloatCC::Equal, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::I32Ne | Operator::I64Ne => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().icmp(IntCC::NotEqual, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::F32Ne | Operator::F64Ne => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().fcmp(FloatCC::NotEqual, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::F32Gt | Operator::F64Gt => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().fcmp(FloatCC::GreaterThan, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::F32Ge | Operator::F64Ge => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().fcmp(FloatCC::GreaterThanOrEqual, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::F32Lt | Operator::F64Lt => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().fcmp(FloatCC::LessThan, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
        Operator::F32Le | Operator::F64Le => {
            let arg2 = stack.pop().unwrap();
            let arg1 = stack.pop().unwrap();
            let val = builder.ins().fcmp(FloatCC::LessThanOrEqual, arg1, arg2);
            stack.push(builder.ins().bint(I32, val));
        }
    }
}

/// Deals with a Wasm instruction located in an unreachable portion of the code. Most of them
/// are dropped but special ones like `End` or `Else` signal the potential end of the unreachable
/// portion so the translation state muts be updated accordingly.
fn translate_unreachable_operator(op: &Operator,
                                  builder: &mut FunctionBuilder<Local>,
                                  stack: &mut Vec<Value>,
                                  control_stack: &mut Vec<ControlStackFrame>,
                                  state: &mut TranslationState) {
    // We don't translate because the code is unreachable
    // Nevertheless we have to record a phantom stack for this code
    // to know when the unreachable code ends
    match *op {
        Operator::If { ty: _ } |
        Operator::Loop { ty: _ } |
        Operator::Block { ty: _ } => {
            state.phantom_unreachable_stack_depth += 1;
        }
        Operator::End => {
            if state.phantom_unreachable_stack_depth > 0 {
                state.phantom_unreachable_stack_depth -= 1;
            } else {
                // This End corresponds to a real control stack frame
                // We switch to the destination block but we don't insert
                // a jump instruction since the code is still unreachable
                let frame = control_stack.pop().unwrap();

                builder.switch_to_block(frame.following_code(), &[]);
                builder.seal_block(frame.following_code());
                match frame {
                    // If it is a loop we also have to seal the body loop block
                    ControlStackFrame::Loop { header, .. } => builder.seal_block(header),
                    // If it is a if then the code after is reachable again
                    ControlStackFrame::If { .. } => {
                        state.real_unreachable_stack_depth = 1;
                    }
                    _ => {}
                }
                if state
                       .br_table_reachable_ebbs
                       .contains(&frame.following_code()) {
                    state.real_unreachable_stack_depth = 1;
                }
                // Now we have to split off the stack the values not used
                // by unreachable code that hasn't been translated
                stack.truncate(frame.original_stack_size());
                // And add the return values of the block but only if the next block is reachble
                // (which corresponds to testing if the stack depth is 1)
                if state.real_unreachable_stack_depth == 1 {
                    stack.extend_from_slice(builder.ebb_args(frame.following_code()));
                }
                state.real_unreachable_stack_depth -= 1;
                state.last_inst_return = false;
            }
        }
        Operator::Else => {
            if state.phantom_unreachable_stack_depth > 0 {
                // This is part of a phantom if-then-else, we do nothing
            } else {
                // Encountering an real else means that the code in the else
                // clause is reachable again
                let (branch_inst, original_stack_size) = match &control_stack[control_stack.len() -
                                                                1] {
                    &ControlStackFrame::If {
                        branch_inst,
                        original_stack_size,
                        ..
                    } => (branch_inst, original_stack_size),
                    _ => panic!("should not happen"),
                };
                // We change the target of the branch instruction
                let else_ebb = builder.create_ebb();
                builder.change_jump_destination(branch_inst, else_ebb);
                builder.seal_block(else_ebb);
                builder.switch_to_block(else_ebb, &[]);
                // Now we have to split off the stack the values not used
                // by unreachable code that hasn't been translated
                stack.truncate(original_stack_size);
                state.real_unreachable_stack_depth = 0;
                state.last_inst_return = false;
            }
        }
        _ => {
            // We don't translate because this is unreachable code
        }
    }
}

fn args_count(index: FunctionIndex,
              functions: &Vec<SignatureIndex>,
              signatures: &Vec<Signature>)
              -> usize {
    signatures[functions[index] as usize].argument_types.len()
}

// Given a index in the function index space, search for it in the function imports and if it is
// not there add it to the function imports.
fn find_function_import(index: FunctionIndex,
                        builder: &mut FunctionBuilder<Local>,
                        func_imports: &mut FunctionImports,
                        functions: &Vec<SignatureIndex>,
                        exports: &Option<HashMap<FunctionIndex, String>>,
                        signatures: &Vec<Signature>)
                        -> FuncRef {
    match func_imports.functions.get(&index) {
        Some(local_index) => return *local_index,
        None => {}
    }
    // We have to import the function
    let sig_index = functions[index];
    match func_imports.signatures.get(&(sig_index as usize)) {
        Some(local_sig_index) => {
            let local_func_index =
                builder.import_function(ExtFuncData {
                                            name: match exports {
                                                &None => FunctionName::new(""),
                                                &Some(ref exports) => {
                                                    match exports.get(&index) {
                                                        None => FunctionName::new(""),
                                                        Some(name) => {
                                                            FunctionName::new(name.clone())
                                                        }
                                                    }
                                                }
                                            },
                                            signature: *local_sig_index,
                                        });
            func_imports.functions.insert(index, local_func_index);
            return local_func_index;
        }
        None => {}
    };
    // We have to import the signature
    let sig_local_index = builder.import_signature(signatures[sig_index as usize].clone());
    func_imports
        .signatures
        .insert(sig_index as usize, sig_local_index);
    let local_func_index =
        builder.import_function(ExtFuncData {
                                    name: match exports {
                                        &None => FunctionName::new(""),
                                        &Some(ref exports) => {
                                            match exports.get(&index) {
                                                None => FunctionName::new(""),
                                                Some(name) => FunctionName::new(name.clone()),
                                            }
                                        }
                                    },
                                    signature: sig_local_index,
                                });
    func_imports.functions.insert(index, local_func_index);
    local_func_index
}

fn find_signature_import(sig_index: SignatureIndex,
                         builder: &mut FunctionBuilder<Local>,
                         func_imports: &mut FunctionImports,
                         signatures: &Vec<Signature>)
                         -> SigRef {
    match func_imports.signatures.get(&(sig_index as usize)) {
        Some(local_sig_index) => return *local_sig_index,
        None => {}
    }
    let sig_local_index = builder.import_signature(signatures[sig_index as usize].clone());
    func_imports
        .signatures
        .insert(sig_index as usize, sig_local_index);
    sig_local_index
}
