//! Executable half of the directory-unit acceptance fixture. It links the
//! sibling cdylib dynamically and prints whatever the library returns.

use std::ffi::CStr;

extern "C" {
    fn unit_answer() -> *const std::os::raw::c_char;
}

fn main() {
    unsafe {
        println!("{}", CStr::from_ptr(unit_answer()).to_str().unwrap());
    }
}
