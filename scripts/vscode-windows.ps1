# Lists VS Code windows, or closes those whose title contains -Close and, when -Without
# is given, does not contain -Without (dev testing helper).
param([string]$Close = "", [string]$Without = "")
Add-Type @"
using System; using System.Text; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class Win {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  public static List<KeyValuePair<IntPtr,string>> List(HashSet<uint> pids) {
    var r = new List<KeyValuePair<IntPtr,string>>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p);
      if (pids.Contains(p) && IsWindowVisible(h)) { var s = new StringBuilder(512); GetWindowText(h, s, 512);
        if (s.Length > 0) r.Add(new KeyValuePair<IntPtr,string>(h, s.ToString())); } return true; }, IntPtr.Zero);
    return r; }
}
"@
$pids = New-Object 'System.Collections.Generic.HashSet[uint32]'
Get-Process Code -ErrorAction SilentlyContinue | ForEach-Object { [void]$pids.Add([uint32]$_.Id) }
foreach ($w in [Win]::List($pids)) {
  if ($Close -and $w.Value.Contains($Close) -and -not ($Without -and $w.Value.Contains($Without))) { [void][Win]::PostMessage($w.Key, 0x10, [IntPtr]::Zero, [IntPtr]::Zero); "closed: $($w.Value)" }
  else { "open: $($w.Value)" }
}
