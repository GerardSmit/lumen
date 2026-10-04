use super::{shadow, SHADOW_BYTES};
use crate::interpreter::Interp;

#[test]
fn engines_on_one_thread_do_not_share_direct_call_storage() {
    let mut first = Interp::new();
    let mut second = Interp::new();
    let left = shadow(&mut first);
    let right = shadow(&mut second);
    assert_ne!(left, right);
    unsafe {
        assert_ne!((*left).top, (*right).top);
        assert_eq!((*left).end - (*left).top, SHADOW_BYTES);
        assert_eq!((*right).end - (*right).top, SHADOW_BYTES);
        // A live first-engine call record must survive a second engine's use of
        // the same physical thread. A TLS stack aliases these words.
        let first_record = (*left).top as *mut usize;
        let second_record = (*right).top as *mut usize;
        first_record.write(0xabc);
        second_record.write(0xdef);
        assert_eq!(first_record.read(), 0xabc);
        assert_eq!(second_record.read(), 0xdef);
    }
}

#[test]
fn shadow_owner_survives_interpreter_moves_and_other_engine_teardown() {
    let mut original = Interp::new();
    let pointer = shadow(&mut original);
    let initial_top = unsafe { (*pointer).top };
    let mut moved = Box::new(original);
    assert_eq!(shadow(&mut moved), pointer);
    let mut other = Interp::new();
    assert_ne!(shadow(&mut other), pointer);
    drop(other);
    unsafe {
        assert_eq!((*pointer).top, initial_top);
        // The frame descriptor remains live until its own interpreter drops.
        assert_eq!((*pointer).end - (*pointer).top, SHADOW_BYTES);
    }
}

#[test]
fn small_shadow_budget_falls_back_when_a_frame_would_cross_its_end() {
    let mut interpreter = Interp::new();
    interpreter.jit_shadow = Some(super::new_shadow(64 << 10));
    let pointer = shadow(&mut interpreter);
    let initial_top = unsafe { (*pointer).top };
    assert_eq!(initial_top % 16, 0);
    assert_eq!(unsafe { (*pointer).end - initial_top }, 64 << 10);
    let program = crate::parser::parse_script("function answer() { return 42; }", false).unwrap();
    let crate::ast::Stmt::FuncDecl(function) = &program[0] else {
        panic!("function declaration");
    };
    let chunk = crate::bytecode::compile(function).unwrap();
    chunk.jit.dentry.set(1); // The capacity check never executes this sentinel.
    chunk.jit.dsize.set(4096);
    unsafe {
        (*pointer).top = (*pointer).end - 4096;
    }
    assert!(super::direct_ready(&mut interpreter, &chunk));
    unsafe {
        (*pointer).top += 16;
    }
    assert!(!super::direct_ready(&mut interpreter, &chunk));
    unsafe {
        (*pointer).top = initial_top;
    }
    assert!(super::direct_ready(&mut interpreter, &chunk));
}

#[test]
fn cold_interpreter_scripts_do_not_allocate_shadow_storage() {
    let mut engine = crate::Engine::new();
    assert!(engine.interp.jit_shadow.is_none());
    engine
        .eval(
            "console.log('ready'); JSON.stringify({runtime:'Lumen'})",
            false,
        )
        .unwrap();
    assert!(engine.interp.jit_shadow.is_none());
}
