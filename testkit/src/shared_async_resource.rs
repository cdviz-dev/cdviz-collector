/// Creates a shared async resource with automatic cleanup (call the drop)
/// It's like a sigleton for test (errors are managed with `.expect(...)`).
/// We used to shared testcontainer between test, and drop them at end
///
/// # Example
/// ```
/// shared_async_resource!(MailBoxes, MailBoxes::new(), get_shared_mailboxes);
/// shared_async_resource!(Database, Database::connect("..."), get_shared_database);
/// ```
///
/// To use it you require to not forbid unsafe, and some dependencies:
/// ```toml
/// [dev-dependencies]
/// ctor = "0.6"
/// paste = "1"
/// tokio = { version = "1", features = ["full"] }
/// ...
/// [lints.rust]
/// unsafe_code = "deny"
/// ```
#[macro_export]
macro_rules! shared_async_resource {
    ($type:ty, $init:expr, $getter_name:ident) => {
        paste::paste! {
            pub struct [<$type Guard>](std::sync::RwLockReadGuard<'static, Option<$type>>);

            impl std::ops::Deref for [<$type Guard>] {
                type Target = $type;

                fn deref(&self) -> &Self::Target {
                    self.0.as_ref().expect(concat!(stringify!($type), " was dropped"))
                }
            }

            static [<SHARED_ $type:upper>]: tokio::sync::OnceCell<std::sync::RwLock<Option<$type>>> =
                tokio::sync::OnceCell::const_new();
            static [<CLEANUP_RUNTIME_ $type:upper>]: std::sync::OnceLock<tokio::runtime::Runtime> =
                std::sync::OnceLock::new();

            pub async fn $getter_name() -> [<$type Guard>] {
                let rwlock = [<SHARED_ $type:upper>]
                    .get_or_init(|| async {
                        let resource = $init.await;
                        std::sync::RwLock::new(Some(resource))
                    })
                    .await;
                [<$type Guard>](rwlock.read().unwrap())
            }

            #[ctor::dtor]
            #[allow(unsafe_code)]
            fn [<cleanup_ $type:snake>]() {
                if let Some(rwlock) = [<SHARED_ $type:upper>].get() {
                    let rt = [<CLEANUP_RUNTIME_ $type:upper>]
                        .get_or_init(|| tokio::runtime::Runtime::new()
                            .expect("Failed to create cleanup runtime"));
                    let _guard = rt.enter();
                    if let Ok(mut guard) = rwlock.write() {
                        drop(guard.take());
                    }
                }
            }
        }
    };
}
