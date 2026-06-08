//! Pure parser for the JOBOBJECT_BASIC_PROCESS_ID_LIST byte buffer (x64 layout). Kept ungated +
//! free of `windows` types so it runs on the host under Miri — the buffer handling is the part with
//! the alignment/UB risk; the QueryInformationJobObject syscall that fills the buffer can't be Miri'd.

/// Parse a JOBOBJECT_BASIC_PROCESS_ID_LIST byte buffer into its PID list. x64 layout:
/// `[0..4]` NumberOfAssignedProcesses (u32) · `[4..8]` NumberOfProcessIdsInList (u32) ·
/// `[8..]` ProcessIdList: one `ULONG_PTR` (8 bytes) per entry. Uses `from_ne_bytes` (safe — no
/// pointer casts, no alignment assumption) and never reads past `buf`.
pub fn parse_job_pid_list(buf: &[u8]) -> Vec<u32> {
    if buf.len() < 8 {
        return Vec::new();
    }
    let count = u32::from_ne_bytes(buf[4..8].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(count.min(buf.len() / 8));
    for i in 0..count {
        let off = 8 + i * 8;
        if off + 8 > buf.len() {
            break; // defensive: the header claimed more pids than the buffer holds
        }
        // ULONG_PTR is 8 bytes on x64 Windows (the only target); low 32 bits are the PID.
        out.push(u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap()) as u32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a little-endian JOBOBJECT_BASIC_PROCESS_ID_LIST byte buffer for `pids`.
    fn make_buf(assigned: u32, in_list: u32, pids: &[u64]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&assigned.to_ne_bytes());
        b.extend_from_slice(&in_list.to_ne_bytes());
        for &p in pids {
            b.extend_from_slice(&p.to_ne_bytes());
        }
        b
    }

    #[test]
    fn parses_several_pids() {
        let buf = make_buf(3, 3, &[1000, 2000, 3000]);
        assert_eq!(parse_job_pid_list(&buf), vec![1000, 2000, 3000]);
    }
    #[test]
    fn empty_list() {
        assert_eq!(parse_job_pid_list(&make_buf(0, 0, &[])), Vec::<u32>::new());
    }
    #[test]
    fn truncates_high_bits_of_ulong_ptr() {
        // a ULONG_PTR with junk in the high 32 bits still yields the low-32 PID
        let buf = make_buf(1, 1, &[0xDEAD_BEEF_0000_1234]);
        assert_eq!(parse_job_pid_list(&buf), vec![0x0000_1234]);
    }
    #[test]
    fn defensive_short_buffer_does_not_overread() {
        // header claims 5 pids but only 2 are present — must stop at the buffer end, no panic/OOB
        let mut buf = make_buf(5, 5, &[10, 20]);
        let _ = &mut buf;
        assert_eq!(parse_job_pid_list(&buf), vec![10, 20]);
    }
    #[test]
    fn too_short_for_header() {
        assert_eq!(parse_job_pid_list(&[0u8; 3]), Vec::<u32>::new());
    }
}
