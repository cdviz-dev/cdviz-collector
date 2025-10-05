use super::Pipe;
use crate::errors::Result;
use std::fmt::Debug;
use std::marker::PhantomData;

// #[derive(Debug, Deserialize, Serialize, Default)]
// pub(crate) struct Config {}

pub(crate) struct Processor<I, N> {
    next: N,
    input_type: PhantomData<I>,
}

impl<I, N> Processor<I, N> {
    pub(crate) fn new(next: N) -> Self {
        Self { next, input_type: PhantomData }
    }
}

impl<I, N> Pipe for Processor<I, N>
where
    I: Debug,
    N: Pipe<Input = I>,
{
    type Input = I;
    fn send(&mut self, input: Self::Input) -> Result<()> {
        self.next.send(input)
    }
}

#[cfg(test)]
mod tests {
    use crate::pipes::collect_to_vec::Collector;

    use super::*;

    #[test]
    fn test_passthrough() {
        let collector = Collector::<i32>::new();
        let mut pipe = Processor::new(collector.create_pipe());
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
