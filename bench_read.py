"""大文件读取测试：测挂载进程内存峰值与读取耗时。"""
import ctypes
import sys
import threading
import time
from ctypes import wintypes


class PMC(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD),
        ("PageFaultCount", wintypes.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
    ]


def find_mount_pid(name=b"ftpmount-rs.exe"):
    k = ctypes.WinDLL("kernel32", use_last_error=True)

    class PE32(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD),
            ("cntUsage", wintypes.DWORD),
            ("th32ProcessID", wintypes.DWORD),
            ("th32DefaultHeapID", ctypes.POINTER(ctypes.c_ulong)),
            ("th32ModuleID", wintypes.DWORD),
            ("cntThreads", wintypes.DWORD),
            ("th32ParentProcessID", wintypes.DWORD),
            ("pcPriClassBase", ctypes.c_long),
            ("dwFlags", wintypes.DWORD),
            ("szExeFile", ctypes.c_char * 260),
        ]

    snap = k.CreateToolhelp32Snapshot(0x2, 0)
    e = PE32()
    e.dwSize = ctypes.sizeof(PE32)
    found = []
    if k.Process32First(snap, ctypes.byref(e)):
        while True:
            if e.szExeFile.lower() == name:
                found.append(e.th32ProcessID)
            if not k.Process32Next(snap, ctypes.byref(e)):
                break
    k.CloseHandle(snap)
    return found[0] if found else None


def rss_mb(pid):
    k = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    h = k.OpenProcess(0x1000, False, pid)
    if not h:
        return None
    pmc = PMC()
    pmc.cb = ctypes.sizeof(PMC)
    ok = psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb)
    k.CloseHandle(h)
    return pmc.WorkingSetSize / (1024 * 1024) if ok else None


path = sys.argv[1] if len(sys.argv) > 1 else r"R:\big.bin"
pid = find_mount_pid()
before = rss_mb(pid) if pid else None
print("PID=%s 读前内存=%.1f MB" % (pid, before or 0))

peak = [before or 0]
stop = threading.Event()


def sampler():
    while not stop.is_set():
        m = rss_mb(pid) if pid else None
        if m and m > peak[0]:
            peak[0] = m
        time.sleep(0.1)


threading.Thread(target=sampler, daemon=True).start()

t0 = time.time()
n = 0
with open(path, "rb") as f:
    while True:
        b = f.read(1 << 20)
        if not b:
            break
        n += len(b)
dt = time.time() - t0
stop.set()

print("读回 %d 字节 用时 %.2f s (%.1f MB/s)" % (n, dt, n / 1048576 / dt))
print("读内存峰值 %.1f MB (增长 %.1f MB)" % (peak[0], peak[0] - (before or 0)))
