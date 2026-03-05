use ctru::prelude::Apt;
use std::{cmp, mem, panic};

pub unsafe fn spawn_system_core_thread(
    apt: &mut Apt,
    f: impl FnOnce() + Send + 'static,
) -> std::io::Result<libc::pthread_t> {
    apt.set_app_cpu_time_limit(30)
        .expect("Failed to enable system core");

    // Most of this implementation has been copied and modified from std.

    let stack = 2 * 1024 * 1024; // DEFAULT_MIN_STACK_SIZE
    let main = Box::new(move || {
        let try_result = panic::catch_unwind(panic::AssertUnwindSafe(f));

        if let Err(err) = try_result {
            log::error!("thread panicked: {err:?}");
        }
    });

    // SAFETY: dynamic size and alignment of the Box remain the same. See below for why the
    // lifetime change is justified.
    let main =
        unsafe { Box::from_raw(Box::into_raw(main) as *mut (dyn FnOnce() + Send + 'static)) };
    let p = Box::into_raw(Box::new(main));
    let mut native: libc::pthread_t = unsafe { mem::zeroed() };
    let mut attr: mem::MaybeUninit<libc::pthread_attr_t> = mem::MaybeUninit::uninit();
    assert_eq!(unsafe { libc::pthread_attr_init(attr.as_mut_ptr()) }, 0);

    let stack_size = cmp::max(stack, libc::PTHREAD_STACK_MIN);

    match unsafe { libc::pthread_attr_setstacksize(attr.as_mut_ptr(), stack_size) } {
        0 => {}
        n => {
            assert_eq!(n, libc::EINVAL);
            // EINVAL means |stack_size| is either too small or not a
            // multiple of the system page size. Because it's definitely
            // >= PTHREAD_STACK_MIN, it must be an alignment issue.
            // Round up to the nearest page and try again.
            // FIXME: Replaced libc::_SC_PAGESIZE with value 8 to avoid error fixed in https://github.com/rust-lang/libc/pull/4875
            let page_size = unsafe { libc::sysconf(8) as usize };
            let stack_size =
                (stack_size + page_size - 1) & (-(page_size as isize - 1) as usize - 1);
            assert_eq!(
                unsafe { libc::pthread_attr_setstacksize(attr.as_mut_ptr(), stack_size) },
                0
            );
        }
    };

    // This is the 3DS specific part
    assert_eq!(
        unsafe { libc::pthread_attr_setprocessorid_np(attr.as_mut_ptr(), 1) },
        0
    );
    let sched_param = libc::sched_param {
        // Set to a higher priority than normal threads (0x30)
        sched_priority: 0x19,
    };
    assert_eq!(
        unsafe { libc::pthread_attr_setschedparam(attr.as_mut_ptr(), &sched_param) },
        0
    );

    let ret =
        unsafe { libc::pthread_create(&mut native, attr.as_ptr(), thread_start, p as *mut _) };
    // Note: if the thread creation fails and this assert fails, then p will
    // be leaked. However, an alternative design could cause double-free
    // which is clearly worse.
    assert_eq!(unsafe { libc::pthread_attr_destroy(attr.as_mut_ptr()) }, 0);

    return if ret != 0 {
        // The thread failed to start and as a result p was not consumed. Therefore, it is
        // safe to reconstruct the box so that it gets deallocated.
        unsafe {
            drop(Box::from_raw(p));
        }
        Err(std::io::Error::from_raw_os_error(ret))
    } else {
        Ok(native)
    };

    extern "C" fn thread_start(main: *mut libc::c_void) -> *mut libc::c_void {
        unsafe {
            // Next, set up our stack overflow handler which may get triggered if we run
            // out of stack.
            // let _handler = stack_overflow::Handler::new();
            // Finally, let's run some code.
            Box::from_raw(main as *mut Box<dyn FnOnce()>)();
        }
        std::ptr::null_mut()
    }
}
