#![deny(unused_variables, unreachable_code)]


use anyhow::{
    Result,
    Error,
    anyhow,
    bail,
};
use misc_utils::{
    SlotMap,
    Key,
};
use std::{
    hash::{
        Hasher,
        Hash,
    },
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
    error::Error as ErrorTrait,
    result::Result as StdResult,
    collections::VecDeque,
    fs::read_to_string,
    path::PathBuf,
    rc::Rc,
};
use crate::{
    ast::{
        Expr as RefExpr,
        FnSignature as RefFnSignature,
        Vector as RefVector,
        Fn as RefFn,
    },
    error_trace,
};
use super::{
    FxIndexMap,
    FxIndexSet,
    DEFAULT_GLOBALS,
};


const IS_TAIL: bool = true;
const NOT_TAIL: bool = false;


#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Instruction {
    Nop,

    Exit,

    ReturnModule,
    Module(ModuleId),

    Func(FnId),

    SetVar(VarSlot),
    SetPath(VarSlot, Rc<Vec<Ident>>),
    GetVar(VarSlot),

    Field(Ident),

    Number(i64),
    Float(f64),
    String(Rc<String>),
    Char(char),
    Bool(bool),
    Byte(u8),
    Ident(Ident),
    None,

    Splat,

    /// Checks if the first data in the scope is callable. If so, then it calls it with the
    /// arguments. If not, then it throws an error.
    Call(usize),
    TailCall(usize),
    Return,

    /// Start a scope with the given var slots
    Scope(usize),
    /// End a scope with the given var slots
    EndScope(usize),

    /// Reads previous result
    JumpIfTrue(InstructionId),
    JumpIfFalse(InstructionId),
    Jump(InstructionId),
}
#[derive(Debug, PartialEq)]
pub enum FnSignature {
    Single {
        params: Vector,
        body_ptr: InstructionId,
    },
    Multi {
        exact: FxIndexMap<usize, (Vector, InstructionId)>,
        max_exact: usize,
        at_least: FxIndexMap<usize, (Vector, InstructionId)>,
        any: Option<(Vector, InstructionId)>,
    },
}
impl FnSignature {
    pub fn match_arg_count(&self, count: usize)->Option<(&Vector, InstructionId)> {
        match self {
            Self::Single{params, body_ptr}=>{
                if params.items.len() > count {
                    return None;
                }
                if params.items.len() < count && params.remainder.is_none() {
                    return None;
                }

                return Some((params, *body_ptr));
            },
            Self::Multi{exact, max_exact, at_least, any}=>{
                if count <= *max_exact {
                    for (param_count, (params, body_ptr)) in exact.iter() {
                        if count == *param_count {
                            return Some((params, *body_ptr));
                        }
                    }
                }

                for (min_param_count, (params, body_ptr)) in at_least.iter() {
                    if count >= *min_param_count {
                        return Some((params, *body_ptr));
                    }
                }

                if let Some((params, body_ptr)) = any {
                    return Some((params, *body_ptr));
                }

                return None;
            },
        }
    }
}


#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct VarSlot {
    pub id: usize,
    pub global: bool,
}
impl Hash for VarSlot {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.id.hash(hasher);
        self.global.hash(hasher);
    }
}

#[derive(Debug)]
pub struct ModuleError;
impl ErrorTrait for ModuleError {}
impl Display for ModuleError {
    fn fmt(&self, f: &mut Formatter)->FmtResult {
        write!(f, "Module error")
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct FnId(usize);
impl Hash for FnId {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        hasher.write_usize(self.0);
    }
}
impl Key for FnId {
    fn from_id(id: usize)->Self {FnId(id)}
    fn id(&self)->usize {self.0}
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct InstructionId(usize);
impl Hash for InstructionId {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        hasher.write_usize(self.0);
    }
}
#[allow(dead_code)]
impl InstructionId {
    pub const fn invalid()->Self {
        InstructionId(usize::MAX)
    }

    pub const fn is_valid(&self)->bool {
        self.0 != usize::MAX
    }

    pub const fn inner(&self)->usize {self.0}
}

#[derive(Debug, PartialEq)]
pub struct Vector {
    pub items: Vec<Ident>,
    pub remainder: Option<Ident>,
}

#[derive(Debug, PartialEq)]
pub struct Fn {
    pub id: FnId,
    pub name: Option<Ident>,
    pub captures: Vec<Ident>,
    pub sig: FnSignature,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ident(pub usize);
impl Hash for Ident {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        hasher.write_usize(self.0);
    }
}

#[derive(Debug)]
pub struct Interner(FxIndexSet<String>);
impl Interner {
    pub fn new()->Self {
        Interner(FxIndexSet::default())
    }

    pub fn intern<S: Into<String>>(&mut self, s: S)->Ident {
        Ident(self.0.insert_full(s.into()).0)
    }

    pub fn get(&self, i: Ident)->&str {
        self.0.get_index(i.0)
            .expect("Invalid interned ident passed")
    }
}

pub struct InstructionStore {
    /// Immutable list of instructions. Nothing gets deleted from here.
    instructions: Vec<Instruction>,

    /// A list of instruction indices describing the order that they execute. Things CAN be removed
    /// from here.
    ins_order: FxIndexSet<InstructionId>,
}
#[allow(dead_code)]
impl InstructionStore {
    pub fn new()->Self {
        InstructionStore {
            instructions: Vec::new(),
            ins_order: FxIndexSet::default(),
        }
    }

    pub fn get_mut(&mut self, id: InstructionId)->&mut Instruction {
        assert!(id.is_valid() && id.0 < self.instructions.len());

        &mut self.instructions[id.0]
    }

    pub fn set(&mut self, id: InstructionId, ins: Instruction) {
        assert!(id.is_valid(), "The given `InstructionId` is invalid");
        assert!(id.0 < self.instructions.len(), "The given `InstructionId` is out of bounds");

        self.instructions[id.0] = ins;
    }

    pub fn next_id(&self)->InstructionId {
        let idx = self.instructions.len();
        assert!(idx < usize::MAX, "Max instruction count reached!");
        InstructionId(idx)
    }

    pub fn current_id(&self)->InstructionId {
        let idx = self.instructions.len() - 1;
        InstructionId(idx)
    }

    pub fn push(&mut self, ins: Instruction)->InstructionId {
        let id = self.next_id();

        self.instructions.push(ins);
        self.ins_order.insert(id);

        return id;
    }

    pub fn insert_after(&mut self, after_id: InstructionId, ins: Instruction)->InstructionId {
        let id = self.next_id();
        let before_idx = self.ins_order.get_index_of(&after_id).expect("Invalid key");

        self.instructions.push(ins);
        self.ins_order.shift_insert(before_idx + 1, id);

        return id;
    }

    /// Inserts the instruction before the instruction with the given id
    pub fn insert_before(&mut self, at_id: InstructionId, ins: Instruction)->InstructionId {
        let id = self.next_id();
        let idx = self.ins_order.get_index_of(&at_id).expect("Invalid key");

        self.instructions.push(ins);
        self.ins_order.shift_insert(idx, id);

        return id;
    }

    pub fn iter(&self)->InstructionIter {
        InstructionIter {
            inner: self,
            index: 0,
        }
    }
}

pub struct InstructionIter<'a> {
    inner: &'a InstructionStore,
    index: usize,
}
#[allow(dead_code)]
impl<'a> InstructionIter<'a> {
    pub fn jump(&mut self, id: InstructionId) {
        let index = self.inner.ins_order
            .get_index_of(&id)
            .expect("Invalid ID");

        self.index = index;
    }

    /// The next id to execute.
    /// Example: [... current, next, ...]
    pub fn next_ins_id(&self)->Option<InstructionId> {
        self.inner.ins_order.get_index(self.index).copied()
    }

    /// The id of the currently executing instruction.
    /// Example: [... current, next, ...]
    pub fn cur_ins_id(&self)->Option<InstructionId> {
        self.inner.ins_order.get_index(self.index.saturating_sub(1)).copied()
    }

    pub fn peek(&self)->&Instruction {
        let id = self.inner.ins_order.get_index(self.index).unwrap();

        &self.inner.instructions[id.0]
    }
}
impl<'a> Iterator for InstructionIter<'a> {
    type Item = &'a Instruction;
    fn next(&mut self)->Option<Self::Item> {
        let id = self.inner.ins_order.get_index(self.index)?;
        self.index += 1;
        return Some(&self.inner.instructions[id.0]);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ModuleId(usize);
impl ModuleId {
    pub const fn root()->Self {
        ModuleId(0)
    }
}
impl Hash for ModuleId {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        hasher.write_usize(self.0);
    }
}
impl Key for ModuleId {
    fn from_id(id: usize)->Self {ModuleId(id)}
    fn id(&self)->usize {self.0}
}

/// Tracking for a global context
pub struct VarState {
    globals: FxIndexSet<Ident>,
    scopes: Vec<VarScope>,
    scope_var_count: usize,
}
impl VarState {
    pub fn new(interner: &mut Interner)->Self {
        let mut globals = FxIndexSet::default();
        for global in DEFAULT_GLOBALS {
            globals.insert(interner.intern(*global));
        }


        return VarState {
            globals,
            scopes: Vec::new(),
            scope_var_count: 0,
        };
    }

    pub fn reset(&mut self) {
        self.globals.drain(DEFAULT_GLOBALS.len()..);
        self.scopes.clear();
    }

    pub fn reset_local(&mut self) {
        self.scopes.clear();
    }

    pub fn insert(&mut self, name: Ident, interner: &Interner)->Result<VarSlot> {
        if self.scopes.len() == 0 {
            if self.globals.contains(&name) {
                bail!("Global {} already exists", interner.get(name));
            }

            let id = self.globals.insert_full(name).0;

            return Ok(VarSlot {
                id,
                global: true,
            });
        } else {
            let scope = self.scopes.last_mut().unwrap();
            if let Some(offset) = scope.vars.get_index_of(&name) {
                return Ok(VarSlot {
                    id: offset + scope.start_slot,
                    global: false,
                });
            }

            self.scope_var_count += 1;
            let offset = scope.vars.insert_full(name).0;
            return Ok(VarSlot {
                id: offset + scope.start_slot,
                global: false,
            });
        }
    }

    pub fn push_scope(&mut self, ins_id: InstructionId) {
        self.scopes.push(VarScope {
            ins_id,
            start_slot: self.scope_var_count,
            vars: FxIndexSet::default(),
        });
    }

    pub fn pop_scope(&mut self)->(InstructionId, usize) {
        let scope = self.scopes.pop().unwrap();
        self.scope_var_count -= scope.vars.len();

        return (scope.ins_id, scope.vars.len());
    }

    pub fn get(&self, name: Ident)->Option<VarSlot> {
        for scope in self.scopes.iter().rev() {
            if let Some(offset) = scope.vars.get_index_of(&name) {
                return Some(VarSlot {
                    id: offset + scope.start_slot,
                    global: false,
                });
            }
        }

        if let Some(offset) = self.globals.get_index_of(&name) {
            return Some(VarSlot {
                id: offset,
                global: true,
            });
        }

        return None;
    }
}

pub struct VarScope {
    ins_id: InstructionId,
    start_slot: usize,
    vars: FxIndexSet<Ident>,
}

pub struct ConvertState {
    pub interner: Interner,
    pub fns: SlotMap<FnId, Rc<Fn>>,
    pub warnings: Vec<Error>,
    pub instructions: InstructionStore,
    pub modules: ModuleTree,
    pub vars: VarState,
}
#[allow(dead_code)]
impl ConvertState {
    pub fn new()->Self {
        let mut interner = Interner::new();
        let vars = VarState::new(&mut interner);

        ConvertState {
            interner,
            fns: SlotMap::new(),
            warnings: Vec::new(),
            instructions: InstructionStore::new(),
            modules: ModuleTree::new(),
            vars,
        }
    }

    pub fn def_var(&mut self, name: &str)->Result<(Ident, VarSlot)> {
        let name = self.intern(name);
        return Ok((name, self.vars.insert(name, &self.interner)?));
    }

    pub fn def_var_ident(&mut self, name: Ident)->Result<VarSlot> {
        return Ok(self.vars.insert(name, &self.interner)?);
    }

    pub fn lookup_var(&mut self, name: &str)->Option<VarSlot> {
        let name = self.intern(name);
        self.vars.get(name)
    }

    #[inline]
    pub fn intern(&mut self, s: &str)->Ident {
        self.interner.intern(s)
    }

    #[inline]
    pub fn warning(&mut self, err: Error) {
        self.warnings.push(err);
    }

    #[inline]
    pub fn call(&mut self, arg_count: usize) {
        self.instructions.push(Instruction::Call(arg_count));
    }

    #[inline]
    pub fn tail_call(&mut self, arg_count: usize) {
        self.instructions.push(Instruction::TailCall(arg_count));
    }

    #[inline]
    pub fn push_return(&mut self) {
        self.instructions.push(Instruction::Return);
    }

    #[inline]
    pub fn push_module_return(&mut self) {
        self.instructions.push(Instruction::ReturnModule);
    }

    #[inline]
    pub fn set_var(&mut self, slot: VarSlot) {
        self.instructions.push(Instruction::SetVar(slot));
    }

    #[inline]
    pub fn set_path(&mut self, slot: VarSlot, path: Vec<Ident>) {
        self.instructions.push(Instruction::SetPath(slot, Rc::new(path)));
    }

    #[inline]
    pub fn get_var(&mut self, slot: VarSlot) {
        self.instructions.push(Instruction::GetVar(slot));
    }

    pub fn dot_ident(&mut self, i: &str) {
        let ident = self.intern(i);

        self.instructions.push(Instruction::Ident(ident));
    }

    #[inline]
    pub fn function(&mut self, f: FnId) {
        self.instructions.push(Instruction::Func(f));
    }

    #[inline]
    pub fn string(&mut self, s: String) {
        self.instructions.push(Instruction::String(Rc::new(s)));
    }

    #[inline]
    pub fn number(&mut self, n: i64) {
        self.instructions.push(Instruction::Number(n));
    }

    #[inline]
    pub fn float(&mut self, f: f64) {
        self.instructions.push(Instruction::Float(f));
    }

    #[inline]
    pub fn bool(&mut self, val: bool) {
        self.instructions.push(Instruction::Bool(val));
    }

    #[inline]
    pub fn splat(&mut self) {
        self.instructions.push(Instruction::Splat);
    }

    #[inline]
    pub fn jump(&mut self, i: InstructionId) {
        self.instructions.push(Instruction::Jump(i));
    }

    #[inline]
    pub fn jump_if_true(&mut self, i: InstructionId) {
        self.instructions.push(Instruction::JumpIfTrue(i));
    }

    #[inline]
    pub fn jump_if_false(&mut self, i: InstructionId) {
        self.instructions.push(Instruction::JumpIfFalse(i));
    }

    #[inline]
    pub fn push_exit(&mut self) {
        self.instructions.push(Instruction::Exit);
    }

    #[inline]
    pub fn push_none(&mut self) {
        self.instructions.push(Instruction::None);
    }

    #[inline]
    pub fn char(&mut self, c: char) {
        self.instructions.push(Instruction::Char(c));
    }

    #[inline]
    pub fn field(&mut self, name: Ident) {
        self.instructions.push(Instruction::Field(name));
    }

    #[inline]
    pub fn reserve_func(&mut self)->FnId {
        self.fns.reserve_slot()
    }

    /// Start a scope and insert a placeholder
    pub fn start_scope(&mut self) {
        let id = self.instructions.push(Instruction::Scope(0));
        self.vars.push_scope(id);
    }

    /// End a scope, update the start with the var count, and push the ending.
    pub fn end_scope(&mut self) {
        let (id, count) = self.vars.pop_scope();
        *self.instructions.get_mut(id) = Instruction::Scope(count);
        self.instructions.push(Instruction::EndScope(count));
    }

    pub fn reserve_module(&mut self)->ModuleId {
        let m = self.modules.reserve_slot();

        return m;
    }

    #[inline]
    pub fn module(&mut self, id: ModuleId) {
        self.instructions.push(Instruction::Module(id));
    }

    #[inline]
    pub fn next_ins_id(&self)->InstructionId {
        self.instructions.next_id()
    }

    #[inline]
    pub fn cur_ins_id(&self)->InstructionId {
        self.instructions.current_id()
    }
}

#[derive(Debug)]
pub struct ModuleNode {
    pub name: Ident,
    pub children: Vec<ModuleId>,

    pub parent: Option<ModuleId>,

    pub start_ins: InstructionId,
}

pub struct ModuleTree {
    tree: SlotMap<ModuleId, ModuleNode>,
}
impl ModuleTree {
    pub fn new()->Self {
        ModuleTree {
            tree: SlotMap::new(),
        }
    }

    #[inline]
    pub fn reserve_slot(&mut self)->ModuleId {
        self.tree.reserve_slot()
    }

    #[inline]
    pub fn insert_reserved(&mut self, id: ModuleId, node: ModuleNode)->StdResult<(), ModuleNode> {
        self.tree.insert_reserved(id, node)
    }

    pub fn get(&self, id: ModuleId)->&ModuleNode {
        self.tree.get(id).unwrap()
    }
}

struct TodoModule {
    name: String,
    id: ModuleId,
    parent: ModuleId,
    path: PathBuf,
}

struct Todos<'a, 'b> {
    pub fns: VecDeque<(FnId, RefFn<'a>)>,
    pub modules: &'b mut VecDeque<TodoModule>,

    /// Helper to temporarily store the children of the current module
    pub new_modules: Vec<ModuleId>,
    /// Stores the current module
    pub current_module: ModuleId,

    pub module_path: PathBuf,
}
impl<'a, 'b> Todos<'a, 'b> {
    fn new(modules: &'b mut VecDeque<TodoModule>)->Self {
        Todos {
            fns: VecDeque::new(),
            modules,
            new_modules: Vec::new(),
            current_module: ModuleId::root(),
            module_path: PathBuf::new(),
        }
    }

    fn queue_fn(&mut self, id: FnId, f: RefFn<'a>) {
        self.fns.push_back((id, f));
    }

    fn queue_module(&mut self, id: ModuleId, name: &str) {
        self.new_modules.push(id);

        self.modules.push_back(TodoModule {
            name: name.to_string(),
            id,
            parent: self.current_module,
            path: self.module_path.clone(),
        });
    }
}


pub fn convert<'a>(exprs: Vec<RefExpr<'a>>)->Result<ConvertState> {
    let mut state = ConvertState::new();
    let mut module_todos = VecDeque::new();
    let mut todos = Todos::new(&mut module_todos);
    let root_module = state.reserve_module();
    todos.current_module = root_module;

    let start_ins = state.next_ins_id();
    convert_exprs(&mut state, &mut todos, exprs.into_iter(), false)?;

    state.push_exit();

    while let Some((id, f)) = todos.fns.pop_back() {
        state.vars.reset_local();
        convert_fn(&mut state, &mut todos, f, id)?;
    }

    let root_children = todos.new_modules;
    let name = state.intern("root");
    state.modules.insert_reserved(root_module, ModuleNode {
        name,
        children: root_children,
        parent: None,
        start_ins,
    }).unwrap();

    while let Some(todo) = module_todos.pop_back() {
        state.vars.reset();
        convert_module(&mut state, &mut module_todos, todo)?;
    }

    return Ok(state);
}

// pub fn repl_convert<'a>(state: &mut ConvertState, exprs: Vec<RefExpr<'a>>)->Result<InstructionId> {
//     let start_id = state.next_ins_id();
//     let mut module_todos = VecDeque::new();
//     let mut todos = Todos::new(&mut module_todos);
//     convert_exprs(state, &mut todos, exprs, false)?;

//     state.push_exit();

//     while let Some((id, f)) = todos.fns.pop_front() {
//         state.vars.reset_local();
//         convert_fn(state, &mut todos, f, id)?;
//     }

//     while let Some(todo) = module_todos.pop_back() {
//         state.vars.reset();
//         convert_module(state, &mut module_todos, todo)?;
//     }

//     return Ok(start_id);
// }

fn convert_module<'a>(state: &mut ConvertState, module_todos: &'a mut VecDeque<TodoModule>, module_todo: TodoModule)->Result<()> {
    let mut todos = Todos::new(module_todos);

    let name = state.intern(&module_todo.name);

    let mut path = module_todo.path;
    path.push(&module_todo.name);

    todos.module_path = path.clone();
    todos.current_module = module_todo.id;

    let source;
    if path.is_dir() {
        path.push("mod.slp");
        source = read_to_string(&path)?;
    } else {
        path.set_extension("slp");
        source = read_to_string(&path)?;
    }

    let mut parser = crate::parser::new_parser(&source);
    let exprs = match parser.parse_all() {
        Ok(e)=>e,
        Err(e)=>{
            error_trace(e, &source, path.display());
            bail!(ModuleError);
        },
    };
    drop(parser);

    let start_ins = state.next_ins_id();
    if let Err(e) = convert_exprs(state, &mut todos, exprs.into_iter(), NOT_TAIL) {
        error_trace(e, &source, path.display());
        bail!(ModuleError);
    }

    state.push_module_return();

    while let Some((id, f)) = todos.fns.pop_back() {
        state.vars.reset_local();
        if let Err(e) = convert_fn(state, &mut todos, f, id) {
            error_trace(e, &source, path.display());
            bail!(ModuleError);
        }
    }

    let children = todos.new_modules;

    state.modules.insert_reserved(module_todo.id, ModuleNode {
        name,
        parent: Some(module_todo.parent),
        start_ins,
        children,
    }).expect("Module already exists!");

    return Ok(());
}

fn convert_exprs<'a, 'b>(state: &mut ConvertState, todos: &mut Todos<'a, 'b>, exprs: impl ExactSizeIterator<Item = RefExpr<'a>>, is_tail: bool)->Result<()> {
    if exprs.len() == 0 {return Ok(())}

    let last = exprs.len() - 1;
    for (i, expr) in exprs.into_iter().enumerate() {
        let expr_is_tail = (i == last) && is_tail;
        convert_single_expr(state, todos, expr, expr_is_tail)?;
    }

    return Ok(());
}

fn convert_single_expr<'a, 'b>(state: &mut ConvertState, todos: &mut Todos<'a, 'b>, expr: RefExpr<'a>, is_tail: bool)->Result<()> {
    Ok(match expr {
        RefExpr::True=>state.bool(true),
        RefExpr::False=>state.bool(false),
        RefExpr::Number(n)=>state.number(n),
        RefExpr::Float(f)=>state.float(f),
        RefExpr::String(s)=>state.string(s),
        RefExpr::Char(c)=>state.char(c),
        RefExpr::Ident(i)=>{
            let slot = state.lookup_var(i)
                .ok_or(anyhow!("Var {} does not exist", i))?;
            state.get_var(slot)
        },
        RefExpr::DotIdent(i)=>state.dot_ident(i),
        RefExpr::Comment(_)=>{},
        RefExpr::Module(name)=>{
            let id = state.reserve_module();
            state.module(id);
            todos.queue_module(id, name);
        },
        RefExpr::Def{name, data}=>{
            convert_single_expr(state, todos, *data, is_tail)?;

            let (_, slot) = state.def_var(name)?;
            state.set_var(slot);
        },
        RefExpr::Set{name, data}=>{
            convert_single_expr(state, todos, *data, is_tail)?;

            let slot = state.lookup_var(name)
                .ok_or(anyhow!("Var {} does not exist", name))?;
            state.set_var(slot);
        },
        RefExpr::SetPath{path, data}=>{
            convert_single_expr(state, todos, *data, is_tail)?;

            let mut path_iter = path.into_iter();
            let name = path_iter.next().unwrap();
            let slot = state.lookup_var(name)
                .ok_or(anyhow!("Var {} does not exist", name))?;

            let path = path_iter.map(|n|state.intern(n)).collect::<Vec<_>>();
            state.set_path(slot, path);
        },
        RefExpr::Object(_)=>panic!("Not supported in the new interpreter!"),
        RefExpr::Path(path)=>{
            let mut path_iter = path.into_iter();
            let var = path_iter.next().unwrap();
            let slot = state.lookup_var(var)
                .ok_or(anyhow!("Var {} does not exist", var))?;
            state.get_var(slot);

            for name in path_iter {
                let i = state.intern(name);
                state.field(i);
            }
        },
        RefExpr::Fn(f)=>{
            let id = state.reserve_func();
            todos.queue_fn(id, f);

            state.function(id);
        },
        RefExpr::Cond{conditions, default}=>{
            state.start_scope();

            let mut jump_ends = Vec::new();
            let mut prev_jf: Option<InstructionId> = None;

            // convert the conditions, storing the locations where final jumps should go, and
            // setting inter-condition jumps as needed
            for (condition, body) in conditions {
                if let Some(id) = prev_jf {
                    let this_id = state.next_ins_id();
                    state.instructions.set(id, Instruction::JumpIfFalse(this_id));
                }

                convert_single_expr(state, todos, condition, NOT_TAIL)?;

                let id = state.instructions.push(Instruction::Exit);
                prev_jf = Some(id);

                convert_single_expr(state, todos, body, is_tail)?;

                if is_tail {
                    state.push_return();
                } else {
                    let id = state.instructions.push(Instruction::Exit);

                    jump_ends.push(id);
                }
            }

            // if there were conditions, set the last if-false jump
            if let Some(id) = prev_jf {
                let this_id = state.next_ins_id();
                state.instructions.set(id, Instruction::JumpIfFalse(this_id));
            }

            if let Some(default) = default {
                convert_single_expr(state, todos, *default, is_tail)?;
                if is_tail {
                    state.push_return();
                }
            }

            if !is_tail {
                // set all of the jump-after-body instructions for the conditions
                let id = state.next_ins_id();
                let ins = Instruction::Jump(id);

                for loc in jump_ends {
                    state.instructions.set(loc, ins.clone());
                }
            } else {
                assert!(jump_ends.is_empty());
            }
        },
        RefExpr::Splat(expr)=>{
            convert_single_expr(state, todos, *expr, NOT_TAIL)?;
            state.splat();
        },
        RefExpr::Begin(exprs)=>{
            state.start_scope();

            convert_exprs(state, todos, exprs.into_iter(), is_tail)?;

            state.end_scope();
        },
        RefExpr::List(exprs)=>{
            let arg_count = exprs.len() - 1;
            state.start_scope();
            let mut exprs_iter = exprs.into_iter();

            let first = exprs_iter.next().unwrap();

            convert_exprs(state, todos, exprs_iter.rev(), is_tail)?;

            convert_single_expr(state, todos, first, NOT_TAIL)?;

            state.end_scope();

            if is_tail {
                state.tail_call(arg_count);
            } else {
                state.call(arg_count);
            }
        },
        RefExpr::None=>state.push_none(),
        RefExpr::Quote(_)=>todo!("Quote conversion"),
        RefExpr::Vector(_)=>todo!("Vector conversion"),
        RefExpr::Squiggle(_)=>todo!("Squiggle conversion"),
        RefExpr::ReplDirective(_)=>bail!("Repl directives are not allowed here!"),
    })
}

fn convert_fn<'a, 'b>(state: &mut ConvertState, todos: &mut Todos<'a, 'b>, func: RefFn<'a>, id: FnId)->Result<()> {
    let name = func.name.map(|n|state.intern(n));
    let captures = func.captures
        .map(|c|c.items
            .into_iter()
            .map(|s|state.intern(s))
            .collect::<Vec<_>>()
        )
        .unwrap_or_default();

    // TODO: Actually implement this thing
    if captures.len() > 0 {
        todo!("Function captures");
    }

    let sig = convert_signature(state, todos, func.signature, &captures)?;

    state.fns.insert_reserved(id, Rc::new(Fn {
        id,
        name,
        captures,
        sig,
    })).unwrap();
    return Ok(());
}

fn def_func_cap_params(state: &mut ConvertState, caps: &[Ident], params: &Vector)->Result<()> {
    for cap in caps {
        state.def_var_ident(*cap)?;
    }
    for param in params.items.iter() {
        state.def_var_ident(*param)?;
    }
    if let Some(rem) = params.remainder {
        state.def_var_ident(rem)?;
    }

    return Ok(());
}

fn convert_signature<'a, 'b>(state: &mut ConvertState, todos: &mut Todos<'a, 'b>, sig: RefFnSignature<'a>, captures: &[Ident])->Result<FnSignature> {
    match sig {
        RefFnSignature::Single(params, body)=>{
            let params = convert_vector(state, params);

            def_func_cap_params(state, captures, &params)?;

            let body_ptr = state.next_ins_id();
            convert_exprs(state, todos, body.into_iter(), IS_TAIL)?;
            state.push_return();

            return Ok(FnSignature::Single{params, body_ptr});
        },
        RefFnSignature::Multi(items)=>{
            let mut exact = FxIndexMap::default();
            let mut max_exact = 0;
            let mut at_least = FxIndexMap::default();
            let mut any = None;

            for (params, body) in items {
                state.vars.reset_local();

                let params = convert_vector(state, params);

                def_func_cap_params(state, captures, &params)?;

                let body_ptr = state.next_ins_id();
                convert_exprs(state, todos, body.into_iter(), IS_TAIL)?;
                state.push_return();

                if params.remainder.is_some() {
                    if params.items.len() == 0 {
                        any = Some((params, body_ptr));
                    } else {
                        at_least.insert(params.items.len(), (params, body_ptr));
                    }
                } else {
                    max_exact = max_exact.max(params.items.len());
                    exact.insert(params.items.len(), (params, body_ptr));
                }
            }

            return Ok(FnSignature::Multi {
                exact,
                max_exact,
                at_least,
                any,
            });
        },
    }
}

fn convert_vector<'a>(state: &mut ConvertState, vector: RefVector<'a>)->Vector {
    let mut items = Vec::new();
    let mut remainder = None;

    for i in vector.items {
        items.push(state.intern(i));
    }

    if let Some(rem) = vector.remainder {
        remainder = Some(state.intern(rem));
    }

    return Vector {items, remainder};
}
