"""大文件写入测试：测挂载进程内存峰值与写入耗时。"""
import ctypes
import os
import sys
import threading
import time
from ctypes import wintypes

TARGET = sys.argv[1] if len(sys.argv) > 1 else r"R:\big.bin"
SIZE_MB = int(sys.argv[2]) if len(sys.argv) > 2 else 256
CHUNK = 1 << 20


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
    """枚举进程找挂载进程 PID。"""
    k = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    TH32CS_SNAPPROCESS = 0x2

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

    snap = k.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
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
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    h = k.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return None
    pmc = PMC()
    pmc.cb = ctypes.sizeof(PMC)
    ok = psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb)
    k.CloseHandle(h)
    return pmc.WorkingSetSize / (1024 * 1024) if ok else None


pid = find_mount_pid()
print("挂载进程 PID:", pid)
base = rss_mb(pid) if pid else None
print("写入前内存: %.1f MB" % base if base else "写入前内存: 未知")

peak = [base or 0]
stop = threading.Event()


def sampler():
    while not stop.is_set():
        m = rss_mb(pid) if pid else None
        if m and m > peak[0]:
            peak[0] = m
        time.sleep(0.15)


t = threading.Thread(target=sampler, daemon=True)
t.start()

buf = bytes(CHUNK)
total = SIZE_MB * CHUNK
t0 = time.time()
with open(TARGET, "wb") as f:
    written = 0
    while written < total:
        f.write(buf)
        written += CHUNK
t_write = time.time() - t0

# close() 触发上传，单独计时
t1 = time.time()
elapsed_close = time.time() - t1

stop.set()
t.join(timeout=1)

size = os.path.getsize(TARGET)
print("写入 %d MB 用时: %.2f s  (%.1f MB/s)" % (SIZE_MB, t_write, SIZE_MB / t_write))
print("内存峰值: %.1f MB  (增长 %.1f MB)" % (peak[0], peak[0] - (base or 0)))
print("挂载点看到的大小: %d 字节 (期望 %d)" % (size, total))
