//! `_random`: the Mersenne Twister generator behind `random.Random`, on `lumen_common::mt19937`.

/// Module implements the Mersenne Twister random number generator.
#[lumen_bind::module(name = "_random")]
pub mod _random {
    use crate::bind::{type_object, KwArgs, Py, This};
    use crate::object::*;
    use crate::pyint::BigInt;
    use crate::vm::Interp;
    use lumen_common::mt19937::{Mt19937, N};

    /// Random() -> create a random number generator with its own internal state.
    #[class(name = "Random")]
    pub struct Random {
        mt: Mt19937,
    }

    fn seed_key(it: &mut Interp, arg: &Value) -> R<Vec<u32>> {
        if arg.is_none() {
            let mut buf = [0u8; N * 4];
            it.platform.borrow_mut().entropy(&mut buf);
            return Ok(buf
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect());
        }
        let magnitude = match arg.as_bigint() {
            Some(b) => b.abs(),
            None => BigInt::from_u64(it.hash_value(arg)? as u64),
        };
        let words = magnitude.words().1;
        let mut key: Vec<u32> = words
            .iter()
            .flat_map(|w| [*w as u32, (*w >> 32) as u32])
            .collect();
        while key.len() > 1 && key.last() == Some(&0) {
            key.pop();
        }
        if key.is_empty() {
            key.push(0);
        }
        Ok(key)
    }

    fn seed_with(slf: &Py<Random>, it: &mut Interp, arg: &Value) -> R<()> {
        let key = seed_key(it, arg)?;
        slf.borrow_mut(it)?.mt.init_by_array(&key);
        Ok(())
    }

    #[methods]
    impl Random {
        #[constructor]
        fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Random {
            let _ = (args, kwargs);
            Random { mt: Mt19937::new() }
        }

        #[proto(init)]
        fn __init__(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[varargs] args: &[Value],
            #[varkw] kwargs: KwArgs,
        ) -> R<()> {
            let exact = {
                let base = type_object::<Random>(it);
                let t = it.type_of(slf.0.value());
                std::rc::Rc::ptr_eq(&t, &base)
            };
            if exact && !kwargs.is_empty() {
                return Err(it.type_error("Random() takes no keyword arguments"));
            }
            if args.len() > 1 {
                return Err(it.type_error("Random() requires 0 or 1 argument"));
            }
            seed_with(&slf.0, it, args.first().unwrap_or(&Value::None))
        }

        /// seed([n]) -> None.
        ///
        /// Defaults to use urandom and falls back to a combination
        /// of the current time and the process identifier.
        fn seed(slf: This<Py<Self>>, it: &mut Interp, n: Option<&Value>) -> R<()> {
            seed_with(&slf.0, it, n.unwrap_or(&Value::None))
        }

        /// random() -> x in the interval [0, 1).
        fn random(&mut self) -> f64 {
            self.mt.next_f64()
        }

        /// getrandbits(k) -> x.  Generates an int with k random bits.
        fn getrandbits(&mut self, it: &mut Interp, k: i32) -> R<Value> {
            if k < 0 {
                return Err(it.value_error("number of bits must be non-negative"));
            }
            if k == 0 {
                return Ok(Value::Int(0));
            }
            let words = self.mt.random_bits(k as u64);
            if words.len() == 1 {
                return Ok(Value::Int(words[0] as i64));
            }
            let wide: Vec<u64> = words
                .chunks(2)
                .map(|c| c[0] as u64 | (c.get(1).copied().unwrap_or(0) as u64) << 32)
                .collect();
            Ok(Value::big(BigInt::from_words(false, wide)))
        }

        /// getstate() -> tuple containing the current state.
        fn getstate(&self) -> Value {
            let (state, index) = self.mt.state();
            Value::tuple(
                state
                    .iter()
                    .map(|w| Value::Int(*w as i64))
                    .chain(std::iter::once(Value::Int(index as i64)))
                    .collect(),
            )
        }

        /// setstate(state) -> None.  Restores generator state.
        fn setstate(&mut self, it: &mut Interp, state: &Value) -> R<()> {
            let Some(items) = state.tuple_items() else {
                return Err(it.type_error("state vector must be a tuple"));
            };
            if items.len() != N + 1 {
                return Err(it.value_error("state vector is the wrong size"));
            }
            let mut words = [0u32; N];
            for (slot, item) in words.iter_mut().zip(items) {
                let Some(b) = item.as_bigint() else {
                    return Err(it.type_error("an integer is required"));
                };
                if b.is_negative() {
                    return Err(it.overflow_err("can't convert negative value to unsigned int"));
                }
                match b.words().1 {
                    [] => *slot = 0,
                    [w] if *w <= u32::MAX as u64 => *slot = *w as u32,
                    _ => {
                        return Err(
                            it.overflow_err("Python int too large to convert to C unsigned long")
                        )
                    }
                }
            }
            let Some(index) = items[N].as_bigint() else {
                return Err(it.type_error("an integer is required"));
            };
            let Some(index) = index.to_i64() else {
                return Err(it.overflow_err("Python int too large to convert to C long"));
            };
            if !(0..=N as i64).contains(&index) {
                return Err(it.value_error("invalid state"));
            }
            self.mt.set_state(words, index as usize);
            Ok(())
        }
    }
}
