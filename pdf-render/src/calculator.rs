use alloc::vec::Vec;
use pdf_core::{DocError, LimitKind, Limits};

use crate::display::{bad, check, refusal};

const STACK: usize = 128;
const DEPTH: u64 = 64;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cell {
    Number(f64),
    Bool(bool),
    Procedure(usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Operator {
    Add, Sub, Mul, Div, Mod, Abs, Neg, Floor, Cvi,
    Dup, Pop, Exch, Index, Roll, Copy,
    Eq, Ne, Lt, Le, Gt, Ge, If, IfElse,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Instruction {
    Push(Cell),
    Open(usize),
    Close,
    Operator(Operator),
}

/// The calculator subset needed by function-based text shadings.
/// Unsupported operators are refused at load time, including in untaken branches.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Calculator {
    code: Vec<Instruction>,
    limits: Limits,
}

impl Calculator {
    pub(crate) fn parse(input: &[u8], limits: Limits) -> Result<Self, DocError> {
        check(LimitKind::DecodedBytes, input.len() as u64, limits.max_decoded_bytes)?;
        let mut code = Vec::new();
        let mut opens = Vec::new();
        let mut offset = 0;
        while let Some(byte) = input.get(offset).copied() {
            if whitespace(byte) {
                offset += 1;
                continue;
            }
            if byte == b'%' {
                while input.get(offset).is_some_and(|byte| !matches!(*byte, b'\r' | b'\n')) {
                    offset += 1;
                }
                continue;
            }
            let instruction = match byte {
                b'{' => {
                    check(LimitKind::Depth, opens.len() as u64 + 1,
                        u64::from(limits.max_depth).min(DEPTH))?;
                    push(&mut opens, code.len(), limits, input.len().saturating_add(
                        code.capacity().saturating_mul(core::mem::size_of::<Instruction>()),
                    ))?;
                    offset += 1;
                    Instruction::Open(0)
                }
                b'}' => {
                    let start = opens.pop().ok_or_else(|| bad("calculator_braces"))?;
                    let end = code.len();
                    let Some(slot) = code.get_mut(start) else { return Err(bad("calculator_braces")) };
                    *slot = Instruction::Open(end);
                    offset += 1;
                    Instruction::Close
                }
                _ => {
                    let start = offset;
                    while input.get(offset).is_some_and(|byte| {
                        !whitespace(*byte) && !matches!(*byte, b'{' | b'}' | b'%')
                    }) {
                        offset += 1;
                    }
                    let token = input.get(start..offset).ok_or_else(|| bad("calculator_token"))?;
                    check(LimitKind::ObjectBytes, token.len() as u64, limits.max_object_bytes)?;
                    match token {
                        b"true" => Instruction::Push(Cell::Bool(true)),
                        b"false" => Instruction::Push(Cell::Bool(false)),
                        _ => {
                            if let Some(number) = core::str::from_utf8(token).ok()
                                .and_then(|text| text.parse::<f64>().ok())
                            {
                                if !number.is_finite() { return Err(bad("calculator_number")); }
                                Instruction::Push(Cell::Number(number))
                            } else {
                                Instruction::Operator(operator(token)?)
                            }
                        }
                    }
                }
            };
            check(LimitKind::Tokens, code.len() as u64 + 1,
                u64::from(limits.max_tokens_per_object))?;
            push(&mut code, instruction, limits, input.len().saturating_add(
                opens.capacity().saturating_mul(core::mem::size_of::<usize>()),
            ))?;
        }
        if !opens.is_empty() { return Err(bad("calculator_braces")); }
        match (code.first(), code.last()) {
            (Some(Instruction::Open(end)), Some(Instruction::Close))
                if *end == code.len().saturating_sub(1) => {}
            _ => return Err(bad("calculator_program")),
        }
        Ok(Self { code, limits })
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.code.capacity().saturating_mul(core::mem::size_of::<Instruction>())
    }

    pub(crate) fn evaluate(
        &self,
        input: [f64; 2],
        outputs: usize,
        work: &mut u64,
    ) -> Result<[f64; 32], DocError> {
        if outputs == 0 || outputs > 32 { return Err(bad("calculator_outputs")); }
        let mut stack = Stack {
            cells: [Cell::Number(0.0); STACK],
            len: 0,
            maximum: usize::try_from(self.limits.max_tokens_per_object)
                .unwrap_or(usize::MAX).min(STACK),
        };
        for value in input { stack.number(value)?; }
        self.run(1, self.code.len().saturating_sub(1), 1, &mut stack, work)?;
        if stack.len != outputs { return Err(bad("calculator_outputs")); }
        let mut result = [0.0; 32];
        for (slot, cell) in result.iter_mut().zip(stack.cells.iter()).take(outputs) {
            let Cell::Number(value) = cell else { return Err(bad("calculator_output_type")) };
            *slot = *value;
        }
        Ok(result)
    }

    fn charge(&self, work: &mut u64, amount: u64) -> Result<(), DocError> {
        *work = work.checked_add(amount).ok_or_else(|| {
            refusal(LimitKind::ContentOps, u64::MAX, self.limits.max_content_ops)
        })?;
        check(LimitKind::ContentOps, *work, self.limits.max_content_ops)
    }

    fn run(
        &self, mut pc: usize, end: usize, depth: u64,
        stack: &mut Stack, work: &mut u64,
    ) -> Result<(), DocError> {
        check(LimitKind::Depth, depth, u64::from(self.limits.max_depth).min(DEPTH))?;
        while pc < end {
            self.charge(work, 1)?;
            let instruction = *self.code.get(pc).ok_or_else(|| bad("calculator_program"))?;
            pc += 1;
            match instruction {
                Instruction::Push(value) => stack.push(value)?,
                Instruction::Open(close) => {
                    if close < pc || close >= end { return Err(bad("calculator_program")); }
                    stack.push(Cell::Procedure(pc, close))?;
                    pc = close + 1;
                }
                Instruction::Close => return Err(bad("calculator_program")),
                Instruction::Operator(op) => match op {
                    Operator::Add | Operator::Sub | Operator::Mul | Operator::Div => {
                        let b = stack.pop_number()?;
                        let a = stack.pop_number()?;
                        let result = match op {
                            Operator::Add => a + b,
                            Operator::Sub => a - b,
                            Operator::Mul => a * b,
                            _ => a / b,
                        };
                        stack.number(result)?;
                    }
                    Operator::Mod => {
                        let b = stack.integer()?;
                        let a = stack.integer()?;
                        if b == 0 { return Err(bad("calculator_mod")); }
                        stack.number(f64::from(a) % f64::from(b))?;
                    }
                    Operator::Abs | Operator::Neg | Operator::Floor | Operator::Cvi => {
                        let value = stack.pop_number()?;
                        let result = match op {
                            Operator::Abs => value.abs(),
                            Operator::Neg => -value,
                            Operator::Floor => floor(value),
                            _ => {
                                if value <= f64::from(i32::MIN) - 1.0
                                    || value >= f64::from(i32::MAX) + 1.0
                                {
                                    return Err(bad("calculator_cvi"));
                                }
                                f64::from(value as i32)
                            }
                        };
                        stack.number(result)?;
                    }
                    Operator::Dup => {
                        let value = *stack.cells.get(stack.len.checked_sub(1)
                            .ok_or_else(|| bad("calculator_stack_underflow"))?)
                            .ok_or_else(|| bad("calculator_stack_underflow"))?;
                        stack.push(value)?;
                    }
                    Operator::Pop => { stack.pop()?; }
                    Operator::Exch => {
                        if stack.len < 2 { return Err(bad("calculator_stack_underflow")); }
                        stack.cells.swap(stack.len - 1, stack.len - 2);
                    }
                    Operator::Index => {
                        let index = stack.count()?;
                        let index = stack.len.checked_sub(index.saturating_add(1))
                            .ok_or_else(|| bad("calculator_index"))?;
                        let value = *stack.cells.get(index).ok_or_else(|| bad("calculator_index"))?;
                        stack.push(value)?;
                    }
                    Operator::Roll => {
                        let shift = stack.integer()?;
                        let count = stack.count()?;
                        let start = stack.len.checked_sub(count).ok_or_else(|| bad("calculator_roll"))?;
                        self.charge(work, count as u64)?;
                        if count != 0 {
                            let shift = i64::from(shift).rem_euclid(count as i64) as usize;
                            stack.cells.get_mut(start..stack.len)
                                .ok_or_else(|| bad("calculator_roll"))?.rotate_right(shift);
                        }
                    }
                    Operator::Copy => {
                        let count = stack.count()?;
                        let start = stack.len.checked_sub(count).ok_or_else(|| bad("calculator_copy"))?;
                        self.charge(work, count as u64)?;
                        for index in start..stack.len {
                            let value = *stack.cells.get(index).ok_or_else(|| bad("calculator_copy"))?;
                            stack.push(value)?;
                        }
                    }
                    Operator::Eq | Operator::Ne => {
                        let b = stack.pop()?;
                        let a = stack.pop()?;
                        stack.push(Cell::Bool(if op == Operator::Eq { a == b } else { a != b }))?;
                    }
                    Operator::Lt | Operator::Le | Operator::Gt | Operator::Ge => {
                        let b = stack.pop_number()?;
                        let a = stack.pop_number()?;
                        let value = match op {
                            Operator::Lt => a < b,
                            Operator::Le => a <= b,
                            Operator::Gt => a > b,
                            _ => a >= b,
                        };
                        stack.push(Cell::Bool(value))?;
                    }
                    Operator::If | Operator::IfElse => {
                        let otherwise = if op == Operator::IfElse { Some(stack.procedure()?) } else { None };
                        let yes = stack.procedure()?;
                        let Cell::Bool(condition) = stack.pop()? else { return Err(bad("calculator_condition")) };
                        if let Some((start, end)) = if condition { Some(yes) } else { otherwise } {
                            self.run(start, end, depth + 1, stack, work)?;
                        }
                    }
                },
            }
        }
        Ok(())
    }
}

struct Stack {
    cells: [Cell; STACK],
    len: usize,
    maximum: usize,
}

impl Stack {
    fn push(&mut self, value: Cell) -> Result<(), DocError> {
        check(LimitKind::Tokens, self.len as u64 + 1, self.maximum as u64)?;
        let slot = self.cells.get_mut(self.len).ok_or_else(|| bad("calculator_stack"))?;
        *slot = value;
        self.len += 1;
        Ok(())
    }

    fn number(&mut self, value: f64) -> Result<(), DocError> {
        if !value.is_finite() { return Err(bad("calculator_arithmetic")); }
        self.push(Cell::Number(value))
    }

    fn pop(&mut self) -> Result<Cell, DocError> {
        self.len = self.len.checked_sub(1).ok_or_else(|| bad("calculator_stack_underflow"))?;
        self.cells.get(self.len).copied().ok_or_else(|| bad("calculator_stack_underflow"))
    }

    fn pop_number(&mut self) -> Result<f64, DocError> {
        let Cell::Number(value) = self.pop()? else { return Err(bad("calculator_number_type")) };
        Ok(value)
    }

    fn integer(&mut self) -> Result<i32, DocError> {
        let value = self.pop_number()?;
        if value < f64::from(i32::MIN) || value > f64::from(i32::MAX)
            || f64::from(value as i32) != value
        {
            return Err(bad("calculator_integer"));
        }
        Ok(value as i32)
    }

    fn count(&mut self) -> Result<usize, DocError> {
        usize::try_from(self.integer()?).map_err(|_| bad("calculator_count"))
    }

    fn procedure(&mut self) -> Result<(usize, usize), DocError> {
        let Cell::Procedure(start, end) = self.pop()? else { return Err(bad("calculator_procedure")) };
        Ok((start, end))
    }
}

fn operator(token: &[u8]) -> Result<Operator, DocError> {
    Ok(match token {
        b"add" => Operator::Add, b"sub" => Operator::Sub,
        b"mul" => Operator::Mul, b"div" => Operator::Div,
        b"mod" => Operator::Mod, b"abs" => Operator::Abs,
        b"neg" => Operator::Neg, b"floor" => Operator::Floor,
        b"cvi" => Operator::Cvi, b"dup" => Operator::Dup,
        b"pop" => Operator::Pop, b"exch" => Operator::Exch,
        b"index" => Operator::Index, b"roll" => Operator::Roll,
        b"copy" => Operator::Copy, b"eq" => Operator::Eq,
        b"ne" => Operator::Ne, b"lt" => Operator::Lt,
        b"le" => Operator::Le, b"gt" => Operator::Gt,
        b"ge" => Operator::Ge, b"if" => Operator::If,
        b"ifelse" => Operator::IfElse,
        _ => return Err(DocError::Unsupported { feature: "calculator operator" }),
    })
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn floor(value: f64) -> f64 {
    if value.abs() >= 9_007_199_254_740_992.0 { return value; }
    let truncated = (value as i64) as f64;
    if truncated > value { truncated - 1.0 } else { truncated }
}

fn push<T>(values: &mut Vec<T>, value: T, limits: Limits, base: usize) -> Result<(), DocError> {
    let requested = values.len().checked_add(1)
        .and_then(|count| count.checked_mul(core::mem::size_of::<T>()))
        .and_then(|bytes| bytes.checked_add(base))
        .ok_or_else(|| refusal(LimitKind::DecodedBytes, u64::MAX, limits.max_decoded_bytes))?;
    check(LimitKind::DecodedBytes, requested as u64, limits.max_decoded_bytes.min(isize::MAX as u64))?;
    check(LimitKind::TotalDecodedBytes, requested as u64, limits.max_total_decoded_bytes)?;
    if values.len() == values.capacity() {
        let size = core::mem::size_of::<T>().max(1);
        let maximum = limits.max_decoded_bytes.min(limits.max_total_decoded_bytes)
            .min(isize::MAX as u64).saturating_sub(base as u64) / size as u64;
        let maximum = usize::try_from(maximum).unwrap_or(usize::MAX);
        let capacity = values.capacity().saturating_mul(2).max(16).min(maximum);
        let additional = capacity.saturating_sub(values.len()).max(1);
        values.try_reserve_exact(additional).map_err(|_| {
            refusal(LimitKind::DecodedBytes, limits.max_decoded_bytes.saturating_add(1), limits.max_decoded_bytes)
        })?;
    }
    values.push(value);
    Ok(())
}
