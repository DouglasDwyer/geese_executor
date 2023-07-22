# geese_executor 

[![Crates.io](https://img.shields.io/crates/v/geese_executor.svg)](https://crates.io/crates/geese_executor)
[![Docs.rs](https://docs.rs/geese_executor/badge.svg)](https://docs.rs/geese_executor)

Geese executor is a runtime for futures, integrated into the Geese event system. It
provides the ability to perform asynchronous work and then send the results back via events.
Each time the `geese_executor::notify::Poll` event is raised, each future will be polled
a single time, and any events raised by futures will be broadcast to all other systems.

A simple example of executor usage is provided below.

```rust
use geese::*;
use geese_executor::*;

struct A {
    cancellation: CancellationToken,
    result: Option<i32>
}

impl A {
    async fn do_background_work() -> i32 {
        // Do some awaiting
        42
    }

    fn handle_result(&mut self, future_result: &i32) {
        self.result = Some(*future_result);
    }
}

impl GeeseSystem for A {
    const DEPENDENCIES: Dependencies = dependencies()
        .with::<GeeseExecutor>();

    const EVENT_HANDLERS: EventHandlers<Self> = event_handlers()
        .with(Self::handle_result);

    fn new(ctx: GeeseContextHandle<Self>) -> Self {
        let cancellation = CancellationToken::default();
        let result = None;

        ctx.get::<GeeseExecutor>().spawn_event(Self::do_background_work())
            .with_cancellation(&cancellation);

        Self { cancellation, result }
    }
}

let mut ctx = GeeseContext::default();
ctx.flush(EventQueue::default()
    .with(geese::notify::add_system::<A>())
    .with(geese_executor::notify::Poll)
);
assert_eq!(Some(42), ctx.get::<A>().result);
```