#[allow(dead_code)]
use super::Pipe;
use crate::errors::{miette, Result};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
};

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Config {}

type Buffer<I> = Arc<Mutex<Vec<I>>>;
pub struct Processor<I> {
    // TODO do we need to use Mutex?
    buffer: Buffer<I>,
}

impl<I> Pipe for Processor<I> {
    type Input = I;
    fn send(&mut self, input: Self::Input) -> Result<()> {
        //.lock().unwrap() if mutex
        self.buffer.lock().map_err(|err| miette!("{}", err))?.push(input);
        Ok(())
    }
}

pub struct Collector<I> {
    buffer: Buffer<I>,
}

impl<I> Collector<I>
where
    I: Clone,
{
    #[allow(dead_code)] // mainly use in tests
    pub fn new() -> Self {
        Self { buffer: Arc::new(Mutex::new(vec![])) }
    }

    #[allow(dead_code)] // mainly use in tests
    pub fn create_pipe(&self) -> Processor<I> {
        Processor { buffer: Arc::clone(&self.buffer) }
    }

    #[allow(dead_code)] // mainly use in tests
    pub fn try_into_iter(&self) -> Result<std::vec::IntoIter<I>> {
        self.buffer
            .lock()
            .map_err(|err| miette!("{}", err))
            .map(|buffer| buffer.clone().into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_to_vec() {
        let collector = Collector::<i32>::new();
        let mut pipe = collector.create_pipe();
        pipe.send(1).unwrap();
        pipe.send(2).unwrap();
        pipe.send(3).unwrap();
        let mut iter = collector.try_into_iter().unwrap();
        assert_eq!(iter.next(), Some(1));
        assert_eq!(iter.next(), Some(2));
        assert_eq!(iter.next(), Some(3));
        assert_eq!(iter.next(), None);
    }
}
