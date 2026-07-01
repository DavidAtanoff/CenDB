//! cendb-jit: selective JIT compilation for CenQL queries using Cranelift.

use cendb_executor::{filter_i64_eq, filter_i64_gt, filter_i64_lt, filter_i64_ge, filter_i64_le, filter_i64_ne, SelectionVector, sum_i64, sum_f64};

use cranelift_codegen::ir::{types, AbiParam, MemFlags, InstBuilder};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};

/// The JIT decision: should this query be JIT-compiled?
#[derive(Clone, Debug)]
pub struct JitDecision {
    pub should_jit: bool,
    pub reason: String,
}

/// Heuristic: decide whether to JIT-compile a query.
pub fn should_jit(
    estimated_rows: u64,
    filter_count: usize,
    has_aggregation: bool,
    execution_count: u32,
) -> JitDecision {
    if estimated_rows > 10_000 && filter_count >= 2 {
        return JitDecision {
            should_jit: true,
            reason: format!("large scan ({} rows) with {} filters", estimated_rows, filter_count),
        };
    }

    if has_aggregation && estimated_rows > 5_000 {
        return JitDecision {
            should_jit: true,
            reason: format!("aggregation on {} rows", estimated_rows),
        };
    }

    if execution_count > 100 && estimated_rows > 1_000 {
        return JitDecision {
            should_jit: true,
            reason: format!("hot query ({} executions, {} rows)", execution_count, estimated_rows),
        };
    }

    if estimated_rows <= 100 {
        return JitDecision {
            should_jit: false,
            reason: "point lookup — interpreted path is faster".to_string(),
        };
    }

    JitDecision {
        should_jit: false,
        reason: "query does not meet JIT threshold".to_string(),
    }
}

/// A JIT-compiled filter function using Cranelift.
pub struct JitFilter {
    pub op: JitOp,
    pub value: i64,
    func_ptr: Option<extern "C" fn(*const i64, usize, *mut u32) -> usize>,
}

/// Supported JIT operations.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JitOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Sum,
}

impl JitFilter {
    /// Create a new JIT filter and compile it to native code via Cranelift.
    pub fn new(op: JitOp, value: i64) -> Self {
        let func_ptr = compile_filter(op, value);
        Self { op, value, func_ptr }
    }

    /// Execute the filter on a column of i64 values.
    pub fn execute(&self, col: &[i64]) -> SelectionVector {
        if let Some(f) = self.func_ptr {
            let mut indices = vec![0u32; col.len()];
            let count = f(col.as_ptr(), col.len(), indices.as_mut_ptr());
            indices.truncate(count);
            SelectionVector { indices }
        } else {
            // Fallback to interpreted vectorized executor if compile failed or JIT is Sum
            match self.op {
                JitOp::Eq => filter_i64_eq(col, self.value),
                JitOp::Ne => filter_i64_ne(col, self.value),
                JitOp::Gt => filter_i64_gt(col, self.value),
                JitOp::Ge => filter_i64_ge(col, self.value),
                JitOp::Lt => filter_i64_lt(col, self.value),
                JitOp::Le => filter_i64_le(col, self.value),
                JitOp::Sum => {
                    let sum = sum_i64(col);
                    let mut sv = SelectionVector::new();
                    sv.push(sum as u32);
                    sv
                }
            }
        }
    }

    /// Execute on an f64 column (stored as bit patterns).
    pub fn execute_f64(&self, col: &[i64]) -> f64 {
        match self.op {
            JitOp::Sum => sum_f64(col),
            _ => 0.0,
        }
    }
}

fn compile_filter(op: JitOp, value: i64) -> Option<extern "C" fn(*const i64, usize, *mut u32) -> usize> {
    if op == JitOp::Sum {
        return None;
    }
    
    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").ok()?;
    flag_builder.set("is_pic", "false").ok()?;
    let isa_builder = cranelift_native::builder().ok()?;
    let isa = isa_builder.finish(settings::Flags::new(flag_builder)).ok()?;
    let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let mut module = JITModule::new(builder);
    
    let mut ctx = module.make_context();
    let mut func_ctx = FunctionBuilderContext::new();
    
    let ptr_type = module.target_config().pointer_type();
    ctx.func.signature.params.push(AbiParam::new(ptr_type)); // col_ptr
    ctx.func.signature.params.push(AbiParam::new(ptr_type)); // col_len
    ctx.func.signature.params.push(AbiParam::new(ptr_type)); // out_ptr
    ctx.func.signature.returns.push(AbiParam::new(ptr_type)); // count of selected
    
    let mut builder = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);
    
    let entry_block = builder.create_block();
    let loop_header = builder.create_block();
    let loop_body = builder.create_block();
    let match_block = builder.create_block();
    let next_block = builder.create_block();
    let exit_block = builder.create_block();
    
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    let col_ptr = builder.block_params(entry_block)[0];
    let col_len = builder.block_params(entry_block)[1];
    let out_ptr = builder.block_params(entry_block)[2];
    
    let i_var = Variable::from_u32(0);
    let count_var = Variable::from_u32(1);
    builder.declare_var(i_var, ptr_type);
    builder.declare_var(count_var, ptr_type);
    
    let zero = builder.ins().iconst(ptr_type, 0);
    builder.def_var(i_var, zero);
    builder.def_var(count_var, zero);
    
    builder.ins().jump(loop_header, &[]);
    
    builder.switch_to_block(loop_header);
    let i_val = builder.use_var(i_var);
    let is_end = builder.ins().icmp(IntCC::Equal, i_val, col_len);
    builder.ins().brif(is_end, exit_block, &[], loop_body, &[]);
    
    builder.switch_to_block(loop_body);
    let i_val = builder.use_var(i_var);
    let scale = builder.ins().iconst(ptr_type, 8);
    let offset = builder.ins().imul(i_val, scale);
    let addr = builder.ins().iadd(col_ptr, offset);
    let val = builder.ins().load(types::I64, MemFlags::new(), addr, 0);
    
    let const_val = builder.ins().iconst(types::I64, value);
    let condition = match op {
        JitOp::Eq => builder.ins().icmp(IntCC::Equal, val, const_val),
        JitOp::Ne => builder.ins().icmp(IntCC::NotEqual, val, const_val),
        JitOp::Gt => builder.ins().icmp(IntCC::SignedGreaterThan, val, const_val),
        JitOp::Ge => builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, val, const_val),
        JitOp::Lt => builder.ins().icmp(IntCC::SignedLessThan, val, const_val),
        JitOp::Le => builder.ins().icmp(IntCC::SignedLessThanOrEqual, val, const_val),
        _ => return None,
    };
    builder.ins().brif(condition, match_block, &[], next_block, &[]);
    
    builder.switch_to_block(match_block);
    let count_val = builder.use_var(count_var);
    let scale_out = builder.ins().iconst(ptr_type, 4);
    let offset_out = builder.ins().imul(count_val, scale_out);
    let addr_out = builder.ins().iadd(out_ptr, offset_out);
    
    let i_u32 = builder.ins().ireduce(types::I32, i_val);
    builder.ins().store(MemFlags::new(), i_u32, addr_out, 0);
    
    let one = builder.ins().iconst(ptr_type, 1);
    let next_count = builder.ins().iadd(count_val, one);
    builder.def_var(count_var, next_count);
    builder.ins().jump(next_block, &[]);
    
    builder.switch_to_block(next_block);
    let i_val = builder.use_var(i_var);
    let one = builder.ins().iconst(ptr_type, 1);
    let next_i = builder.ins().iadd(i_val, one);
    builder.def_var(i_var, next_i);
    builder.ins().jump(loop_header, &[]);
    
    builder.switch_to_block(exit_block);
    let final_count = builder.use_var(count_var);
    builder.ins().return_(&[final_count]);
    
    builder.seal_all_blocks();
    builder.finalize();
    
    let func_id = module.declare_function("filter", Linkage::Export, &ctx.func.signature).ok()?;
    module.define_function(func_id, &mut ctx).ok()?;
    module.clear_context(&mut ctx);
    module.finalize_definitions().ok()?;
    
    let code_ptr = module.get_finalized_function(func_id);
    
    // Leak the JITModule to keep compiled code memory mapped.
    std::mem::forget(module);
    
    unsafe {
        let f: extern "C" fn(*const i64, usize, *mut u32) -> usize = std::mem::transmute(code_ptr);
        Some(f)
    }
}

/// A JIT-compiled query plan: a sequence of filters and aggregations.
pub struct JitPlan {
    pub filters: Vec<JitFilter>,
    pub aggregation: Option<JitOp>,
    pub estimated_rows: u64,
}

impl JitPlan {
    /// Execute the plan on a column of data.
    pub fn execute(&self, col: &[i64]) -> (SelectionVector, Option<i64>) {
        let mut sv = SelectionVector::with_capacity(col.len());
        for i in 0..col.len() {
            sv.push(i as u32);
        }

        for filter in &self.filters {
            let filter_sv = filter.execute(col);
            let filter_set: std::collections::HashSet<u32> = filter_sv.indices.into_iter().collect();
            sv.indices.retain(|&idx| filter_set.contains(&idx));
        }

        let agg_result = if let Some(agg) = self.aggregation {
            match agg {
                JitOp::Sum => {
                    let sum: i64 = sv.indices.iter().map(|&i| col[i as usize]).sum();
                    Some(sum)
                }
                _ => None,
            }
        } else {
            None
        };

        (sv, agg_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_decision_large_scan() {
        let d = should_jit(50_000, 3, false, 1);
        assert!(d.should_jit);
    }

    #[test]
    fn jit_decision_point_lookup() {
        let d = should_jit(50, 1, false, 1);
        assert!(!d.should_jit);
    }

    #[test]
    fn jit_decision_hot_query() {
        let d = should_jit(2000, 1, false, 200);
        assert!(d.should_jit);
    }

    #[test]
    fn jit_filter_execution() {
        let filter = JitFilter::new(JitOp::Gt, 50);
        let col: Vec<i64> = vec![10, 20, 60, 80, 30, 90];
        let sv = filter.execute(&col);
        assert_eq!(sv.len(), 3);
        assert_eq!(sv.indices, vec![2, 3, 5]);
    }

    #[test]
    fn jit_plan_with_aggregation() {
        let plan = JitPlan {
            filters: vec![JitFilter::new(JitOp::Ge, 50)],
            aggregation: Some(JitOp::Sum),
            estimated_rows: 1000,
        };
        let col: Vec<i64> = vec![10, 50, 60, 80, 20, 90];
        let (sv, agg) = plan.execute(&col);
        assert_eq!(sv.len(), 4);
        assert_eq!(agg, Some(280));
    }

    #[test]
    fn jit_plan_multiple_filters() {
        let plan = JitPlan {
            filters: vec![
                JitFilter::new(JitOp::Ge, 30),
                JitFilter::new(JitOp::Le, 70),
            ],
            aggregation: None,
            estimated_rows: 1000,
        };
        let col: Vec<i64> = vec![10, 20, 30, 40, 50, 60, 70, 80, 90];
        let (sv, _) = plan.execute(&col);
        assert_eq!(sv.len(), 5);
    }
}
