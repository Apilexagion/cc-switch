//! Write large legacy-keychain credentials without putting secrets in process arguments.
//!
//! Existing generic-password items are updated in place, leaving their ACLs intact.
//! New items trust this executable and `/usr/bin/security`, matching the CLI-based
//! reader without granting access to every application. The legacy public APIs are
//! intentional: they preserve the ACL semantics used by the native CLI credentials.

const ITEM_NOT_FOUND: i32 = -25300;
const DUPLICATE_ITEM: i32 = -25299;

trait PasswordWriter {
    type Item;

    fn find(&self, service: &str, account: &str) -> Result<Option<Self::Item>, i32>;
    fn update(&self, item: &Self::Item, secret: &[u8]) -> Result<(), i32>;
    fn create(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), i32>;
}

fn write_with(
    writer: &impl PasswordWriter,
    service: &str,
    account: &str,
    secret: &[u8],
) -> Result<(), i32> {
    if let Some(item) = writer.find(service, account)? {
        // In particular, an authorization failure must not fall back to deleting
        // and recreating an item with a different ACL.
        return writer.update(&item, secret);
    }
    match writer.create(service, account, secret) {
        // A CLI may have created the same identity after our lookup. Re-read the
        // exact item and preserve its ACL, rather than replacing it.
        Err(DUPLICATE_ITEM) => match writer.find(service, account)? {
            Some(item) => writer.update(&item, secret),
            None => Err(ITEM_NOT_FOUND),
        },
        result => result,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn write_generic_password(
    service: &str,
    account: &str,
    secret: &[u8],
) -> Result<(), crate::error::AppError> {
    write_with(&native::SystemKeychain, service, account, secret).map_err(|status| {
        crate::error::AppError::Message(format!(
            "写入钥匙串失败（macOS 状态 {status}）；请在系统授权提示中允许 CC Switch 访问该条目后重试"
        ))
    })
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::{c_char, c_void};
    use std::ptr;

    use super::{PasswordWriter, ITEM_NOT_FOUND};

    const PARAM_ERROR: i32 = -50;
    const ALLOCATION_ERROR: i32 = -108;
    const UTF8: u32 = 0x0800_0100;
    const GENERIC_PASSWORD: u32 = u32::from_be_bytes(*b"genp");
    const ACCOUNT: u32 = u32::from_be_bytes(*b"acct");
    const SERVICE: u32 = u32::from_be_bytes(*b"svce");

    type Ref = *const c_void;

    #[repr(C)]
    struct Attribute {
        tag: u32,
        length: u32,
        data: *mut c_void,
    }

    #[repr(C)]
    struct AttributeList {
        count: u32,
        attributes: *mut Attribute,
    }

    #[repr(C)]
    struct ArrayCallbacks {
        version: isize,
        retain: Option<unsafe extern "C" fn(Ref, Ref) -> Ref>,
        release: Option<unsafe extern "C" fn(Ref, Ref)>,
        copy_description: Option<unsafe extern "C" fn(Ref) -> Ref>,
        equal: Option<unsafe extern "C" fn(Ref, Ref) -> u8>,
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainFindGenericPassword(
            keychain: Ref,
            service_len: u32,
            service: *const c_char,
            account_len: u32,
            account: *const c_char,
            password_len: *mut u32,
            password: *mut *mut c_void,
            item: *mut Ref,
        ) -> i32;
        fn SecKeychainItemModifyAttributesAndData(
            item: Ref,
            attributes: *const AttributeList,
            length: u32,
            data: *const c_void,
        ) -> i32;
        fn SecKeychainItemCreateFromContent(
            item_class: u32,
            attributes: *mut AttributeList,
            length: u32,
            data: *const c_void,
            keychain: Ref,
            access: Ref,
            item: *mut Ref,
        ) -> i32;
        fn SecTrustedApplicationCreateFromPath(path: *const c_char, app: *mut Ref) -> i32;
        fn SecAccessCreate(description: Ref, trusted_apps: Ref, access: *mut Ref) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeArrayCallBacks: ArrayCallbacks;
        fn CFRelease(value: Ref);
        fn CFArrayCreate(
            allocator: Ref,
            values: *const Ref,
            count: isize,
            callbacks: *const ArrayCallbacks,
        ) -> Ref;
        fn CFStringCreateWithBytes(
            allocator: Ref,
            bytes: *const u8,
            count: isize,
            encoding: u32,
            external_representation: u8,
        ) -> Ref;
    }

    pub(super) struct Owned(Ref);

    impl Owned {
        fn new(value: Ref) -> Result<Self, i32> {
            if value.is_null() {
                Err(ALLOCATION_ERROR)
            } else {
                Ok(Self(value))
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: every Owned comes from a successful Create/Find call and
            // owns exactly one Core Foundation retain. It is never cloned.
            unsafe { CFRelease(self.0) };
        }
    }

    fn success(status: i32) -> Result<(), i32> {
        if status == 0 {
            Ok(())
        } else {
            Err(status)
        }
    }

    fn length(bytes: &[u8]) -> Result<u32, i32> {
        bytes.len().try_into().map_err(|_| PARAM_ERROR)
    }

    fn trusted_app(path: *const c_char) -> Result<Owned, i32> {
        let mut app = ptr::null();
        // SAFETY: path is either null (the current executable) or a static,
        // NUL-terminated path, and app is a valid output pointer.
        success(unsafe { SecTrustedApplicationCreateFromPath(path, &mut app) })?;
        Owned::new(app)
    }

    fn new_access() -> Result<Owned, i32> {
        let current = trusted_app(ptr::null())?;
        let security = trusted_app(c"/usr/bin/security".as_ptr())?;
        let values = [current.0, security.0];
        // SAFETY: values points to two live CF objects; the standard callbacks
        // retain them for the array's lifetime.
        let apps = Owned::new(unsafe {
            CFArrayCreate(
                ptr::null(),
                values.as_ptr(),
                values.len() as isize,
                &kCFTypeArrayCallBacks,
            )
        })?;
        let label = b"CC Switch CLI credentials";
        // SAFETY: label is valid UTF-8 and remains live for the copying call.
        let description = Owned::new(unsafe {
            CFStringCreateWithBytes(ptr::null(), label.as_ptr(), label.len() as isize, UTF8, 0)
        })?;
        let mut access = ptr::null();
        // SAFETY: both input objects are valid and access is a writable output.
        success(unsafe { SecAccessCreate(description.0, apps.0, &mut access) })?;
        Owned::new(access)
    }

    pub(super) struct SystemKeychain;

    impl PasswordWriter for SystemKeychain {
        type Item = Owned;

        fn find(&self, service: &str, account: &str) -> Result<Option<Owned>, i32> {
            let mut item = ptr::null();
            // SAFETY: both UTF-8 buffers live through the call and lengths are
            // checked. Null password outputs mean no secret is copied out.
            let status = unsafe {
                SecKeychainFindGenericPassword(
                    ptr::null(),
                    length(service.as_bytes())?,
                    service.as_ptr().cast(),
                    length(account.as_bytes())?,
                    account.as_ptr().cast(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut item,
                )
            };
            if status == ITEM_NOT_FOUND {
                return Ok(None);
            }
            success(status)?;
            Owned::new(item).map(Some)
        }

        fn update(&self, item: &Owned, secret: &[u8]) -> Result<(), i32> {
            // SAFETY: the retained item and secret buffer remain live. A null
            // attribute list updates only data, preserving all existing ACLs.
            success(unsafe {
                SecKeychainItemModifyAttributesAndData(
                    item.0,
                    ptr::null(),
                    length(secret)?,
                    secret.as_ptr().cast(),
                )
            })
        }

        fn create(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), i32> {
            let access = new_access()?;
            let mut attributes = [
                Attribute {
                    tag: SERVICE,
                    length: length(service.as_bytes())?,
                    data: service.as_ptr().cast_mut().cast(),
                },
                Attribute {
                    tag: ACCOUNT,
                    length: length(account.as_bytes())?,
                    data: account.as_ptr().cast_mut().cast(),
                },
            ];
            let mut list = AttributeList {
                count: attributes.len() as u32,
                attributes: attributes.as_mut_ptr(),
            };
            // SAFETY: all attribute/data buffers and access live through the
            // synchronous copying call; null keychain selects the default one.
            success(unsafe {
                SecKeychainItemCreateFromContent(
                    GENERIC_PASSWORD,
                    &mut list,
                    length(secret)?,
                    secret.as_ptr().cast(),
                    ptr::null(),
                    access.0,
                    ptr::null_mut(),
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    struct Writer {
        findings: RefCell<Vec<Result<Option<u8>, i32>>>,
        create_result: Result<(), i32>,
        update_result: Result<(), i32>,
        calls: RefCell<Vec<(&'static str, Vec<u8>)>>,
    }

    impl PasswordWriter for Writer {
        type Item = u8;

        fn find(&self, _: &str, _: &str) -> Result<Option<u8>, i32> {
            self.calls.borrow_mut().push(("find", vec![]));
            self.findings.borrow_mut().remove(0)
        }

        fn update(&self, _: &u8, secret: &[u8]) -> Result<(), i32> {
            self.calls.borrow_mut().push(("update", secret.to_vec()));
            self.update_result
        }

        fn create(&self, _: &str, _: &str, secret: &[u8]) -> Result<(), i32> {
            self.calls.borrow_mut().push(("create", secret.to_vec()));
            self.create_result
        }
    }

    fn writer(findings: Vec<Result<Option<u8>, i32>>) -> Writer {
        Writer {
            findings: RefCell::new(findings),
            create_result: Ok(()),
            update_result: Ok(()),
            calls: RefCell::new(vec![]),
        }
    }

    #[test]
    fn large_existing_secret_is_updated_in_place() {
        let backend = writer(vec![Ok(Some(1))]);
        let secret = vec![b's'; 32 * 1024];
        write_with(&backend, "service", "account", &secret).unwrap();
        let calls = backend.calls.borrow();
        assert_eq!(
            calls.iter().map(|c| c.0).collect::<Vec<_>>(),
            ["find", "update"]
        );
        assert_eq!(calls[1].1, secret);
    }

    #[test]
    fn authorization_errors_do_not_recreate_an_existing_item() {
        let mut backend = writer(vec![Ok(Some(1))]);
        backend.update_result = Err(-25293);
        assert_eq!(
            write_with(&backend, "service", "account", b"secret"),
            Err(-25293)
        );
        assert_eq!(backend.calls.borrow().len(), 2);
        let backend = writer(vec![Err(-128)]);
        assert_eq!(
            write_with(&backend, "service", "account", b"secret"),
            Err(-128)
        );
        assert_eq!(backend.calls.borrow().len(), 1);
    }

    #[test]
    fn a_concurrent_create_reuses_the_existing_item_without_deleting_it() {
        let mut backend = writer(vec![Ok(None), Ok(Some(2))]);
        backend.create_result = Err(DUPLICATE_ITEM);
        write_with(&backend, "service", "account", b"secret").unwrap();
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .map(|c| c.0)
                .collect::<Vec<_>>(),
            ["find", "create", "find", "update"]
        );
    }
}
