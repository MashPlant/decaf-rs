use crate::{TypeCk, TypeCkTrait};
use common::{ErrorKind::*, Loc, LENGTH, BinOp, UnOp, ErrorKind, Ref};
use syntax::ast::*;
use syntax::{ScopeOwner, Symbol, ty::*};
use std::ops::{Deref, DerefMut};

pub(crate) struct TypePass<'a>(pub TypeCk<'a>);

impl<'a> Deref for TypePass<'a> {
  type Target = TypeCk<'a>;
  fn deref(&self) -> &Self::Target { &self.0 }
}

impl<'a> DerefMut for TypePass<'a> {
  fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
}

impl<'a> TypePass<'a> {
  pub fn program(&mut self, p: &'a Program<'a>) {
    self.scoped(ScopeOwner::Global(p), |s| for c in &p.class { s.class_def(c); });
  }

  fn class_def(&mut self, c: &'a ClassDef<'a>) {
    self.cur_class = Some(c);
    self.scoped(ScopeOwner::Class(c), |s| for f in &c.field {
      if let FieldDef::FuncDef(f) = f {
        s.cur_func = Some(f);
        let t = s.scoped(ScopeOwner::Param(f), |s| s.block(&f.body));
        if !t && f.ret_ty() != Ty::void() {
          s.errors.issue(f.body.loc, ErrorKind::NoReturn)
        }
      };
    });
  }

  // whether this block has a return value depends on the first stmt in this block that has a return value or is a Break
  // it has a return yes => block has, it is a Break => no
  // there is no such stmt => no
  // in addition, if this stmt is not the last stmt, an UnreachableCode error should be reported
  fn block(&mut self, b: &'a Block<'a>) -> bool {
    let mut ret = false;
    let (mut ended, mut issued) = (false, false);
    self.scoped(ScopeOwner::Local(b), |s| for st in &b.stmt {
      if ended && !issued {
        issued = true;
        s.errors.issue(st.loc, ErrorKind::UnreachableCode)
      }
      let t = s.stmt(st);
      if !ended { ret = t; }
      ended = ret || match st.kind { StmtKind::Break(_) => true, _ => false };
    });
    ret
  }

  // return whether this stmt has a return value
  fn stmt(&mut self, s: &'a Stmt<'a>) -> bool {
    match &s.kind {
      StmtKind::Assign(a) => {
        let (l, r) = (self.expr(&a.dst), self.expr(&a.src));
        if l.is_func() || !r.assignable_to(l) {
          self.errors.issue(s.loc, IncompatibleBinary { l, op: "=", r })
        }
        false
      }
      StmtKind::LocalVarDef(v) => {
        self.cur_var_def = Some(v);
        if let Some((loc, e)) = &v.init {
          let (l, r) = (v.ty.get(), self.expr(e));
          if !r.assignable_to(l) {
            self.errors.issue(*loc, IncompatibleBinary { l, op: "=", r })
          }
        }
        self.cur_var_def = None;
        false
      }
      StmtKind::ExprEval(e) => {
        self.expr(e);
        false
      }
      StmtKind::Skip(_) => false,
      StmtKind::If(i) => {
        self.check_bool(&i.cond);
        let s1 = self.block(&i.on_true);
        let s2 = if let Some(of) = &i.on_false { self.block(of) } else { false };
        s1 && s2
      }
      StmtKind::While(w) => {
        self.check_bool(&w.cond);
        self.loop_cnt += 1;
        self.block(&w.body);
        self.loop_cnt -= 1;
        false
      }
      StmtKind::For(f) => self.scoped(ScopeOwner::Local(&f.body), |s| {
        s.stmt(&f.init);
        s.check_bool(&f.cond);
        s.stmt(&f.update);
        for st in &f.body.stmt { s.stmt(st); } // not calling block(), because the scope is already opened
        false
      }),
      StmtKind::Return(r) => {
        let expect = self.cur_func.unwrap().ret_ty();
        if let Some(e) = r {
          let actual = self.expr(e);
          if !actual.assignable_to(expect) {
            self.errors.issue(s.loc, ReturnMismatch { actual, expect })
          }
          true
        } else {
          if expect != Ty::void() {
            self.errors.issue(s.loc, ReturnMismatch { actual: Ty::void(), expect })
          }
          false
        }
      }
      StmtKind::Print(p) => {
        for (i, e) in p.iter().enumerate() {
          let ty = self.expr(e);
          if ty != Ty::error() && ty != Ty::bool() && ty != Ty::int() && ty != Ty::string() {
            self.errors.issue(e.loc, BadPrintArg { loc: i as u32 + 1, ty })
          }
        }
        false
      }
      StmtKind::Break(_) => {
        if self.loop_cnt == 0 { self.errors.issue(s.loc, BreakOutOfLoop) }
        false
      }
      StmtKind::Block(b) => self.block(b),
    }
  }

  // e.ty is set to the return value; e.result is set if e can be statically evaluated
  fn expr(&mut self, e: &'a Expr<'a>) -> Ty<'a> {
    use ExprKind::*;
    let ty = match &e.kind {
      VarSel(v) => self.var_sel(v, e.loc),
      IndexSel(i) => {
        let (arr, idx) = (self.expr(&i.arr), self.expr(&i.idx));
        if idx != Ty::int() && idx != Ty::error() {
          self.errors.issue(e.loc, IndexNotInt)
        }
        match arr {
          Ty { arr, kind } if arr > 0 => Ty { arr: arr - 1, kind },
          e if e == Ty::error() => Ty::error(),
          _ => self.errors.issue(i.arr.loc, IndexNotArray),
        }
      }
      IntLit(_) | ReadInt(_) => Ty::int(),
      BoolLit(_) => Ty::bool(),
      StringLit(_) | ReadLine(_) => Ty::string(),
      NullLit(_) => Ty::null(),
      Call(c) => self.call(c, e.loc),
      Unary(u) => self.unary(u, e.loc),
      Binary(b) => self.binary(b, e.loc),
      This(_) => if !self.cur_func.unwrap().static_ {
        Ty::mk_obj(self.cur_class.unwrap())
      } else { self.errors.issue(e.loc, ThisInStatic) }
      NewClass(n) => match self.scopes.lookup_class(n.name) {
        Some(c) => {
          n.class.set(Some(c));
          Ty::mk_obj(c)
        }
        None => self.errors.issue(e.loc, NoSuchClass(n.name)),
      },
      NewArray(n) => {
        let len = self.expr(&n.len);
        if len != Ty::int() && len != Ty::error() {
          self.errors.issue(n.len.loc, NewArrayNotInt)
        }
        self.ty(&SynTy { loc: n.elem.loc, arr: n.elem.arr + 1, kind: n.elem.kind })
      }
      ClassTest(c) => {
        let src = self.expr(&c.expr);
        if src != Ty::error() && !src.is_object() {
          self.errors.issue(e.loc, NotObject { owner: src })
        }
        match self.scopes.lookup_class(c.name) {
          Some(class) => {
            c.class.set(Some(class));
            Ty::bool()
          }
          None => self.errors.issue(e.loc, NoSuchClass(c.name)),
        }
      }
      ClassCast(c) => {
        let src = self.expr(&c.expr);
        if src != Ty::error() && !src.is_object() {
          self.errors.issue(e.loc, NotObject { owner: src })
        }
        match self.scopes.lookup_class(c.name) {
          Some(class) => {
            c.class.set(Some(class));
            Ty::mk_obj(class)
          }
          None => self.errors.issue(e.loc, NoSuchClass(c.name)),
        }
      }
    };
    e.ty.set(ty);
    ty
  }

  fn binary(&mut self, b: &'a Binary<'a>, loc: Loc) -> Ty<'a> {
    use BinOp::*;
    let (l, r) = (self.expr(&b.l), self.expr(&b.r));
    if l == Ty::error() || r == Ty::error() {
      match b.op {
        Add | Sub | Mul | Div | Mod => Ty::int(),
        And | Or | Eq | Ne | Lt | Le | Gt | Ge => Ty::bool(),
      }
    } else {
      let (ret, ok) = match b.op {
        Add | Sub | Mul | Div | Mod => (Ty::int(), l == Ty::int() && r == Ty::int()),
        Lt | Le | Gt | Ge => (Ty::bool(), l == Ty::int() && r == Ty::int()),
        Eq | Ne => (Ty::bool(), l.assignable_to(r) || r.assignable_to(l)),
        And | Or => (Ty::bool(), l == Ty::bool() && r == Ty::bool())
      };
      if !ok { self.errors.issue(loc, IncompatibleBinary { l, op: b.op.to_op_str(), r }) }
      ret
    }
  }

  fn unary(&mut self, u: &'a Unary<'a>, loc: Loc) -> Ty<'a> {
    let r = self.expr(&u.r);
    match u.op {
      UnOp::Neg => {
        if r != Ty::int() && r != Ty::error() { self.errors.issue(loc, IncompatibleUnary { op: "-", r }) }
        Ty::int()
      }
      UnOp::Not => {
        if r != Ty::bool() && r != Ty::error() { self.errors.issue(loc, IncompatibleUnary { op: "!", r }) }
        Ty::bool()
      }
    }
  }

  fn var_sel(&mut self, v: &'a VarSel<'a>, loc: Loc) -> Ty<'a> {
    // not found(no owner) or sole ClassName => UndeclaredVar
    // refer to field in static function => RefInStatic
    // <not object>.a (Main.a, 1.a, func.a) => BadFieldAssess
    // access a field that doesn't belong to self & ancestors => PrivateFieldAccess
    // given owner but not found object.a => NoSuchField

    match &v.owner {
      Some(o) => {
        self.cur_used = true;
        let o_t = self.expr(o);
        match o_t {
          Ty { arr: 0, kind: TyKind::Object(Ref(c)) } => match c.lookup(v.name) {
            Some(symbol) => {
              match symbol {
                Symbol::Var(var) => {
                  v.var.set(Some(var));
                  // only allow self & descendents to access field
                  if !self.cur_class.unwrap().extends(c) {
                    self.errors.issue(loc, PrivateFieldAccess { name: v.name, owner: o_t })
                  }
                  var.ty.get()
                }
                _ => symbol.ty(),
              }
            }
            None => self.errors.issue(loc, NoSuchField { name: v.name, owner: o_t })
          }
          e if e == Ty::error() => Ty::error(),
          _ => self.errors.issue(loc, BadFieldAccess { name: v.name, owner: o_t }),
        }
      }
      None => {
        // if this expr is in an VarDef, it cannot access the variable that is being declared
        let ret = match self.scopes.lookup_before(v.name, self.cur_var_def.map(|v| v.loc).unwrap_or(loc)) {
          Some(symbol) => match symbol {
            Symbol::Var(var) => {
              v.var.set(Some(var));
              if var.owner.get().unwrap().is_class() {
                let cur = self.cur_func.unwrap();
                if cur.static_ {
                  self.errors.issue(loc, RefInStatic { field: v.name, func: cur.name })
                }
              }
              var.ty.get()
            }
            Symbol::Func(f) => Ty::mk_func(f),
            Symbol::This(f) => Ty::mk_obj(f.class.get().unwrap()),
            Symbol::Class(c) => {
              if !self.cur_used {
                self.errors.issue(loc, UndeclaredVar(v.name))
              } else { Ty::mk_class(c) }
            }
          }
          None => self.errors.issue(loc, UndeclaredVar(v.name)),
        };
        self.cur_used = false;
        ret
      }
    }
  }

  fn call(&mut self, c: &'a Call<'a>, loc: Loc) -> Ty<'a> {
    match &c.owner {
      Some(o) => {
        self.cur_used = true;
        let o_t = self.expr(o);
        if o_t == Ty::error() { return Ty::error(); }
        if c.name == LENGTH && o_t.is_arr() {
          if !c.arg.is_empty() {
            self.errors.issue(loc, LengthWithArgument(c.arg.len() as u32))
          }
          return Ty::int();
        }
        match o_t.kind {
          TyKind::Class(cl) | TyKind::Object(cl) => {
            self.check_call(c, o_t, cl.lookup(c.name), loc)
          }
          _ => self.errors.issue(loc, BadFieldAccess { name: c.name, owner: o_t }),
        }
      }
      None => {
        let cur = self.cur_class.unwrap();
        self.check_call(c, Ty::mk_obj(cur), cur.lookup(c.name), loc)
      }
    }
  }
}

impl<'a> TypePass<'a> {
  fn check_bool(&mut self, e: &'a Expr<'a>) {
    let ty = self.expr(e);
    if ty != Ty::bool() && ty != Ty::error() {
      self.errors.issue(e.loc, TestNotBool)
    }
  }

  fn check_call(&mut self, c: &'a Call<'a>, owner: Ty<'a>, symbol: Option<Symbol<'a>>, loc: Loc) -> Ty<'a> {
    match symbol {
      Some(symbol) => {
        match symbol {
          Symbol::Func(f) => {
            c.func.set(Some(f));
            match &c.owner {
              Some(_) => if owner.is_class() && !f.static_ {
                // call a instance method through class name
                self.errors.issue(loc, BadFieldAccess { name: c.name, owner })
              }
              None => {
                let cur = self.cur_func.unwrap();
                if cur.static_ && !f.static_ {
                  self.errors.issue(loc, RefInStatic { field: f.name, func: cur.name })
                }
              }
            };
            if f.param.len() != c.arg.len() {
              self.errors.issue(loc, ArgcMismatch { name: c.name, expect: f.param.len() as u32, actual: c.arg.len() as u32 })
            } else {
              for (idx, (arg, param)) in c.arg.iter().zip(f.param.iter()).enumerate() {
                let arg = self.expr(arg);
                if !arg.assignable_to(param.ty.get()) {
                  self.errors.issue(c.arg[idx].loc, ArgMismatch { loc: idx as u32 + 1, arg, param: param.ty.get() })
                }
              }
            }
            f.ret_ty()
          }
          _ => self.errors.issue(loc, NotFunc { name: c.name, owner }),
        }
      }
      None => self.errors.issue(loc, NoSuchField { name: c.name, owner })
    }
  }
}
