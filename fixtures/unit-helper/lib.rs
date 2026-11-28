//! Tiny cdylib used by the directory-unit acceptance fixture. The string
//! marker proves the executable really called into the sibling library.

#[no_mangle]
pub extern "C" fn unit_answer() -> *const std::os::raw::c_char {
    b"unit-library-answer\0".as_ptr().cast()
}
