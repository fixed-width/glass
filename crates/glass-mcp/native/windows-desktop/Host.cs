using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Forms;
using GlassDesktopRdp;

internal static class Native {
    [DllImport("wtsapi32.dll", SetLastError=true)] internal static extern bool WTSIsChildSessionsEnabled(out bool enabled);
    [DllImport("wtsapi32.dll", SetLastError=true)] internal static extern bool WTSEnableChildSessions(bool enabled);
    [DllImport("wtsapi32.dll", SetLastError=true)] internal static extern bool WTSGetChildSessionId(out uint session);
    [DllImport("kernel32.dll")] internal static extern uint WTSGetActiveConsoleSessionId();
    [DllImport("wtsapi32.dll", SetLastError=true)] internal static extern bool WTSLogoffSession(IntPtr server,uint session,bool wait);
    [DllImport("wtsapi32.dll", CharSet=CharSet.Unicode,SetLastError=true)] static extern bool WTSEnumerateSessionsW(IntPtr server,uint reserved,uint version,out IntPtr buffer,out uint count);
    [DllImport("wtsapi32.dll", CharSet=CharSet.Unicode,SetLastError=true)] static extern bool WTSQuerySessionInformationW(IntPtr server,uint session,int kind,out IntPtr buffer,out uint bytes);
    [DllImport("wtsapi32.dll")] static extern void WTSFreeMemory(IntPtr buffer);
    [DllImport("kernel32.dll", SetLastError=true)] internal static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
    [DllImport("kernel32.dll")] internal static extern uint WaitForSingleObject(IntPtr handle,uint timeout);
    [DllImport("kernel32.dll")] internal static extern bool CloseHandle(IntPtr handle);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string text,uint revision,out IntPtr descriptor,IntPtr length);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr value);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool MoveFileExW(string source,string destination,uint flags);
    internal static void Publish(string source,string destination) {
        DateTime deadline=DateTime.UtcNow.AddSeconds(2);
        while (!MoveFileExW(source,destination,9)) {
            int code=Marshal.GetLastWin32Error();
            if ((code!=5 && code!=32 && code!=33) || DateTime.UtcNow>=deadline)
                throw new Win32Exception(code,"Publish private journal (Win32 "+code+": "+new Win32Exception(code).Message+")");
            Thread.Sleep(10);
        }
    }
    [StructLayout(LayoutKind.Sequential)] struct SecurityAttributes { public int Length; public IntPtr Descriptor; [MarshalAs(UnmanagedType.Bool)] public bool Inherit; }
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern Microsoft.Win32.SafeHandles.SafeFileHandle CreateFileW(string path,uint access,uint sharing,ref SecurityAttributes security,uint disposition,uint flags,IntPtr template);
    internal static Microsoft.Win32.SafeHandles.SafeFileHandle PrivateFile(string path) {
        IntPtr descriptor;
        string sddl="O:"+WindowsIdentity.GetCurrent().User.Value+"D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)";
        if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl,1,out descriptor,IntPtr.Zero)) throw Error("Create private journal descriptor");
        try {
            var security=new SecurityAttributes { Length=Marshal.SizeOf(typeof(SecurityAttributes)),Descriptor=descriptor,Inherit=false };
            var handle=CreateFileW(path,0x1f01ff,7,ref security,1,0x200080,IntPtr.Zero);
            if (handle.IsInvalid) { handle.Dispose(); throw Error("Create private journal file"); }
            return handle;
        } finally { LocalFree(descriptor); }
    }
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool OpenProcessToken(IntPtr process,uint access,out IntPtr token);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool GetTokenInformation(IntPtr token,int kind,out TokenStatistics statistics,int length,out int returned);
    [DllImport("user32.dll")] internal static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] internal static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] internal static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] internal static extern uint GetWindowThreadProcessId(IntPtr window,out uint process);
    [StructLayout(LayoutKind.Sequential)] struct Luid { public uint Low; public int High; }
    [StructLayout(LayoutKind.Sequential)] struct TokenStatistics {
        public Luid Id,Authentication;
        public long Expiration;
        public int Type,Impersonation;
        public uint DynamicCharged,DynamicAvailable,Groups,Privileges;
        public Luid Modified;
    }
    [StructLayout(LayoutKind.Sequential)] struct Session { public uint Id; public IntPtr Name; public int State; }
    [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Unicode)] struct SessionInfo {
        public int State;
        public uint Session,IncomingBytes,OutgoingBytes,IncomingFrames,OutgoingFrames,IncomingCompressed,OutgoingCompressed;
        [MarshalAs(UnmanagedType.ByValTStr,SizeConst=32)] public string Station;
        [MarshalAs(UnmanagedType.ByValTStr,SizeConst=17)] public string Domain;
        [MarshalAs(UnmanagedType.ByValTStr,SizeConst=21)] public string User;
        public long Connected,Disconnected,LastInput,Logon,Current;
    }
    internal static Exception Error(string operation) {
        int code=Marshal.GetLastWin32Error();
        return new Win32Exception(code,operation+" (Win32 "+code+": "+new Win32Exception(code).Message+")");
    }
    internal static uint Child() {
        uint child;
        if (WTSGetChildSessionId(out child)) return child;
        if (Marshal.GetLastWin32Error()==1168 && child==UInt32.MaxValue) return UInt32.MaxValue;
        throw Error("Read child session");
    }
    internal static List<uint> Sessions() {
        IntPtr buffer; uint count;
        if (!WTSEnumerateSessionsW(IntPtr.Zero,0,1,out buffer,out count)) throw Error("Enumerate Windows sessions");
        try {
            var result=new List<uint>(); int size=Marshal.SizeOf(typeof(Session));
            for (int i=0;i<count;i++) result.Add(((Session)Marshal.PtrToStructure(IntPtr.Add(buffer,i*size),typeof(Session))).Id);
            return result;
        } finally { WTSFreeMemory(buffer); }
    }
    internal static long Logon(uint session) {
        IntPtr buffer; uint size;
        if (!WTSQuerySessionInformationW(IntPtr.Zero,session,24,out buffer,out size)) throw Error("Read child logon generation");
        try {
            if (size<Marshal.SizeOf(typeof(SessionInfo))) throw new InvalidOperationException("Short Windows session response");
            var info=(SessionInfo)Marshal.PtrToStructure(buffer,typeof(SessionInfo));
            if (info.Session!=session) throw new InvalidOperationException("Missing child logon generation");
            return info.Logon;
        } finally { WTSFreeMemory(buffer); }
    }
    internal static bool OnlyParentLogon(uint parent) {
        foreach (uint session in Sessions()) {
            IntPtr buffer; uint count;
            if (!WTSQuerySessionInformationW(IntPtr.Zero,session,5,out buffer,out count)) throw Error("Read session ownership before restoring child setting");
            try {
                string user=Marshal.PtrToStringUni(buffer);
                if (!String.IsNullOrEmpty(user) && session!=parent) return false;
            } finally { WTSFreeMemory(buffer); }
        }
        return true;
    }
    internal static void Token(IntPtr process,out string sid,out ulong logon) {
        IntPtr token;
        if (!OpenProcessToken(process,8,out token)) throw Error("Read desktop process token");
        try {
            using (var identity=new WindowsIdentity(token)) sid=identity.User.Value;
            TokenStatistics statistics; int returned;
            if (!GetTokenInformation(token,10,out statistics,Marshal.SizeOf(typeof(TokenStatistics)),out returned)) throw Error("Read desktop logon identity");
            logon=((ulong)(uint)statistics.Authentication.High<<32)|statistics.Authentication.Low;
        } finally { CloseHandle(token); }
    }
}

public sealed class ProcessRecord {
    public uint pid,session;
    public ulong created,logon;
    public string sid,image;
    internal static ProcessRecord Read(uint pid) {
        using (var process=Process.GetProcessById(checked((int)pid))) {
            if (process.HasExited) throw new InvalidOperationException("Desktop process has exited");
            IntPtr handle=Native.OpenProcess(0x101000,false,pid);
            if (handle==IntPtr.Zero) throw Native.Error("Open desktop process identity");
            try {
                string sid; ulong logon; Native.Token(handle,out sid,out logon);
                return new ProcessRecord { pid=pid,session=(uint)process.SessionId,
                    created=(ulong)process.StartTime.ToUniversalTime().ToFileTimeUtc(),sid=sid,logon=logon,image=process.MainModule.FileName };
            } finally { Native.CloseHandle(handle); }
        }
    }
    internal bool Matches(ProcessRecord other) {
        return pid==other.pid && created==other.created && session==other.session && sid==other.sid && logon==other.logon &&
            String.Equals(image,other.image,StringComparison.OrdinalIgnoreCase);
    }
    internal bool Live() {
        try { return Matches(Read(pid)); }
        catch (ArgumentException) { return false; }
        catch (InvalidOperationException) { return false; }
    }
}

public sealed class Launch {
    public uint version;
    public string instance,pipe,worker_exe;
    public ProcessRecord parent,helper;
}
public sealed class WorkerReady {
    public uint version;
    public string instance;
    public ProcessRecord process;
}
public sealed class Journal {
    public uint version=1,child=UInt32.MaxValue;
    public string instance,phase,error,task,worker_arguments,pipe,stop_event,show_event,hide_event;
    public ProcessRecord parent,owner,worker;
    public long child_logon;
    public bool child_enabled_before,changed_child_setting,clean,viewer_visible,child_gone;
}

internal static class Files {
    static readonly JavaScriptSerializer Json=new JavaScriptSerializer { MaxJsonLength=1024*1024,RecursionLimit=32 };
    internal static T Read<T>(string root,string name) {
        string path=Path.Combine(root,name);
        var info=new FileInfo(path);
        if ((info.Attributes&FileAttributes.ReparsePoint)!=0 || info.Length>1024*1024) throw new InvalidOperationException("Invalid desktop state file");
        using (var stream=new FileStream(path,FileMode.Open,FileAccess.Read,FileShare.ReadWrite|FileShare.Delete))
        using (var reader=new StreamReader(stream)) return Json.Deserialize<T>(reader.ReadToEnd());
    }
    internal static void Write(string root,Journal journal) { WriteText(root,"owner.json",Json.Serialize(journal)); }
    internal static void WriteText(string root,string name,string text) {
        string temporary=Path.Combine(root,"owner-"+Guid.NewGuid().ToString("N")+".tmp"),destination=Path.Combine(root,name);
        byte[] bytes=Encoding.UTF8.GetBytes(text);
        using (var handle=Native.PrivateFile(temporary))
        using (var stream=new FileStream(handle,FileAccess.Write)) {
            stream.Write(bytes,0,bytes.Length); stream.Flush(true);
        }
        Native.Publish(temporary,destination);
    }
    internal static void RequirePrivate(string root) {
        if (!Path.IsPathRooted(root) || (File.GetAttributes(root)&FileAttributes.ReparsePoint)!=0) throw new InvalidOperationException("Desktop state requires an ordinary absolute directory");
        var identity=WindowsIdentity.GetCurrent();
        var security=Directory.GetAccessControl(root);
        if (!security.GetOwner(typeof(SecurityIdentifier)).Equals(identity.User) || !security.AreAccessRulesProtected) throw new InvalidOperationException("Desktop state directory is not private");
        foreach (FileSystemAccessRule rule in security.GetAccessRules(true,true,typeof(SecurityIdentifier))) {
            if (rule.AccessControlType!=AccessControlType.Allow) continue;
            string sid=((SecurityIdentifier)rule.IdentityReference).Value;
            if (sid!=identity.User.Value && sid!="S-1-3-4" && sid!="S-1-5-18") throw new InvalidOperationException("Desktop state permits another account");
        }
    }
    internal static string Quote(string value) {
        var result=new StringBuilder("\""); int slashes=0;
        foreach (char c in value) {
            if (c=='\\') { slashes++; continue; }
            if (c=='\"') result.Append('\\',slashes*2+1); else result.Append('\\',slashes);
            slashes=0; result.Append(c);
        }
        return result.Append('\\',slashes*2).Append('"').ToString();
    }
}

internal sealed class RdpControl : AxHost {
    internal RdpControl() : base("A0C63C30-F08D-4AB4-907C-34905D770C7D") {}
    internal object Client { get { return GetOcx(); } }
}

internal sealed class DesktopHost : Form {
    readonly string root;
    readonly Launch launch;
    readonly Journal state;
    readonly RdpControl host=new RdpControl();
    readonly System.Windows.Forms.Timer timer=new System.Windows.Forms.Timer { Interval=250 };
    readonly DateTime startupDeadline=DateTime.UtcNow.AddSeconds(120);
    readonly EventWaitHandle stop,show,hide;
    readonly IntPtr parentHandle;
    IntPtr previousForeground=Native.GetForegroundWindow();
    uint previousPid;
    IMsRdpClient10 client;
    readonly List<KeyValuePair<int,Delegate>> events=new List<KeyValuePair<int,Delegate>>();
    bool ending,loginComplete,workerStarted;

    internal DesktopHost(string root,Launch launch) {
        this.root=root; this.launch=launch;
        var current=ProcessRecord.Read((uint)Process.GetCurrentProcess().Id);
        if (launch.version!=1 || launch.parent==null || launch.parent.session==0 || launch.parent.session!=current.session ||
            launch.parent.sid!=current.sid || launch.parent.logon!=current.logon || !launch.parent.Live() || launch.helper==null || !launch.helper.Matches(current) ||
            !String.Equals(launch.worker_exe,launch.parent.image,StringComparison.OrdinalIgnoreCase))
            throw new InvalidOperationException("Desktop host is outside its recorded parent logon");
        if (launch.instance==null || !System.Text.RegularExpressions.Regex.IsMatch(launch.instance,"^[a-f0-9]{32}$") || launch.pipe!="glass-desktop-"+launch.instance)
            throw new InvalidOperationException("Invalid desktop launch identity");
        parentHandle=Native.OpenProcess(0x101000,false,launch.parent.pid);
        if (parentHandle==IntPtr.Zero) throw Native.Error("Retain desktop parent identity");
        state=new Journal { instance=launch.instance,parent=launch.parent,owner=current,pipe=launch.pipe,phase="starting",clean=false,
            stop_event="Global\\Glass.Desktop.Stop."+launch.instance,show_event="Local\\Glass.Desktop.Show."+launch.instance,
            hide_event="Local\\Glass.Desktop.Hide."+launch.instance,task="Glass.Desktop."+launch.instance };
        stop=Event(state.stop_event); show=Event(state.show_event); hide=Event(state.hide_event);
        Native.GetWindowThreadProcessId(previousForeground,out previousPid);
        Text="Glass separate desktop - Windows sign-in"; Size=new Size(1024,768);
        StartPosition=FormStartPosition.Manual; Location=new Point(80,80); ShowInTaskbar=true;
        host.Dock=DockStyle.Fill; Controls.Add(host);
        Shown+=Start;
        timer.Tick+=Tick;
        FormClosing+=CloseRequested;
        Files.Write(root,state);
    }
    protected override bool ShowWithoutActivation { get { return true; } }
    static EventWaitHandle Event(string name) {
        var security=new EventWaitHandleSecurity();
        security.SetAccessRuleProtection(true,false);
        security.AddAccessRule(new EventWaitHandleAccessRule(WindowsIdentity.GetCurrent().User,EventWaitHandleRights.FullControl,AccessControlType.Allow));
        bool created;
        var value=new EventWaitHandle(false,name.StartsWith("Global\\Glass.Desktop.Stop.") ? EventResetMode.ManualReset : EventResetMode.AutoReset,name,out created,security);
        if (!created) { value.Dispose(); throw new InvalidOperationException("Desktop control event already exists"); }
        return value;
    }
    void Save(string phase) { state.phase=phase; state.viewer_visible=Visible; Files.Write(root,state); }
    void Listen(string name,Delegate handler) {
        var method=typeof(IMsTscAxEvents).GetMethod(name);
        var id=(DispIdAttribute)Attribute.GetCustomAttribute(method,typeof(DispIdAttribute));
        ComEventsHelper.Combine(host.Client,typeof(IMsTscAxEvents).GUID,id.Value,handler);
        events.Add(new KeyValuePair<int,Delegate>(id.Value,handler));
    }
    void Start(object sender,EventArgs args) {
        try {
            bool enabled;
            if (!Native.WTSIsChildSessionsEnabled(out enabled)) throw Native.Error("Read child-session setting");
            state.child_enabled_before=enabled;
            state.changed_child_setting=false;
            Files.Write(root,state);
            if (!enabled) throw new InvalidOperationException("Child sessions are disabled. Run glass-mcp desktop enable and complete Windows setup first.");
            if (!Native.WTSIsChildSessionsEnabled(out enabled) || !enabled) throw new InvalidOperationException("Child-session enablement did not read back");
            uint previous=Native.Child();
            if (previous!=UInt32.MaxValue) throw new InvalidOperationException("A child session already exists; Glass will not adopt it");
            client=(IMsRdpClient10)host.Client;
            var extended=(IMsRdpExtendedSettings)host.Client;
            object child=true,ignoreMoves=true;
            extended.set_Property("ConnectToChildSession",ref child);
            extended.set_Property("IgnoreServerGeneratedMouseMoves",ref ignoreMoves);
            if (!(bool)extended.get_Property("ConnectToChildSession") || !(bool)extended.get_Property("IgnoreServerGeneratedMouseMoves"))
                throw new InvalidOperationException("Child routing or parent cursor protection was not accepted");
            client.Server="localhost"; client.UserName=Environment.UserName; client.Domain=Environment.MachineName;
            client.DesktopWidth=1280; client.DesktopHeight=900; client.ColorDepth=32;
            client.AdvancedSettings3.EnableAutoReconnect=false;
            client.AdvancedSettings2.DisplayConnectionBar=false;
            client.AdvancedSettings6.RedirectClipboard=false; client.AdvancedSettings6.RedirectDrives=false;
            client.AdvancedSettings6.RedirectPrinters=false; client.AdvancedSettings6.RedirectPorts=false; client.AdvancedSettings6.RedirectSmartCards=false;
            client.AdvancedSettings7.EnableCredSspSupport=true;
            var credentials=(IMsRdpClientNonScriptable5)host.Client;
            credentials.AllowCredentialSaving=false; credentials.AllowPromptingForCredentials=true;
            credentials.PromptForCredentials=true; credentials.PromptForCredsOnClient=true; credentials.DisableConnectionBar=true;
            Listen("OnConnected",new Action(()=>{ if (!loginComplete) Save("authentication_pending"); }));
            Listen("OnLoginComplete",new Action(()=>{ loginComplete=true; Save("starting_desktop"); }));
            Listen("OnDisconnected",new Action<int>(reason=>{ if (!ending) Fail(new InvalidOperationException("Child connection ended ("+reason+", "+client.ExtendedDisconnectReason+")")); }));
            Listen("OnFatalError",new Action<int>(code=>Fail(new InvalidOperationException("RDP control error "+code))));
            timer.Start(); client.Connect();
        } catch (Exception error) { Fail(error); }
    }
    void Tick(object sender,EventArgs args) {
        try {
            if (stop.WaitOne(0) || Native.WaitForSingleObject(parentHandle,0)!=258) { CloseOwned(); return; }
            if (workerStarted && File.Exists(Path.Combine(root,"worker-error.json"))) {
                var failure=Files.Read<Dictionary<string,object>>(root,"worker-error.json");
                if (!failure.ContainsKey("instance") || (string)failure["instance"]!=state.instance) throw new InvalidOperationException("Worker failure record has the wrong launch identity");
                throw new InvalidOperationException("Child worker: "+Convert.ToString(failure["error"]));
            }
            if (show.WaitOne(0) && state.phase=="ready") { previousForeground=Native.GetForegroundWindow(); Native.GetWindowThreadProcessId(previousForeground,out previousPid); Text="Glass separate desktop - close to hide"; Show(); Activate(); Save("ready"); }
            if (hide.WaitOne(0) && state.phase=="ready") { HideAndRestore(); Save("ready"); }
            if (!loginComplete) { if (DateTime.UtcNow>startupDeadline) throw new TimeoutException("Windows child login did not complete within 120 seconds"); return; }
            uint child=Native.Child();
            if (child==UInt32.MaxValue && !workerStarted && DateTime.UtcNow<startupDeadline) return;
            if (child==UInt32.MaxValue || child==0 || child==launch.parent.session) throw new InvalidOperationException("Windows has not provided a separate child session");
            if (!workerStarted) {
                state.child=child; state.child_logon=Native.Logon(child);
                if (state.child_logon<=0 && DateTime.UtcNow<startupDeadline) return;
                if (state.child_logon<=0) throw new InvalidOperationException("Child logon generation did not become available");
                HideAndRestore(); Save("starting_worker");
                StartWorker(); workerStarted=true;
            }
            if (child!=state.child || Native.Logon(child)!=state.child_logon) throw new InvalidOperationException("Child session generation changed");
            if (state.phase=="starting_worker" && File.Exists(Path.Combine(root,"worker.json"))) {
                var worker=Files.Read<WorkerReady>(root,"worker.json");
                if (worker.version!=1 || worker.instance!=launch.instance || worker.process==null || worker.process.session!=child ||
                    worker.process.sid!=launch.parent.sid || !String.Equals(worker.process.image,launch.worker_exe,StringComparison.OrdinalIgnoreCase) || !worker.process.Live())
                    throw new InvalidOperationException("Child worker identity does not match its launch");
                state.worker=worker.process; Save("ready");
            }
            if (state.phase!="ready" && DateTime.UtcNow>startupDeadline) throw new TimeoutException("Child worker did not become ready");
            if (state.worker!=null && !state.worker.Live()) throw new InvalidOperationException("Child worker exited");
        } catch (Exception error) { Fail(error); }
    }
    void StartWorker() {
        state.worker_arguments="windows-desktop-worker --root "+Files.Quote(root)+" --instance "+launch.instance;
        Files.Write(root,state);
        dynamic scheduler=Activator.CreateInstance(Type.GetTypeFromProgID("Schedule.Service",true)); scheduler.Connect();
        dynamic task=scheduler.NewTask(0);
        task.Principal.UserId=launch.parent.sid; task.Principal.LogonType=3; task.Principal.RunLevel=0;
        task.Settings.AllowDemandStart=true; task.Settings.DisallowStartIfOnBatteries=false; task.Settings.StopIfGoingOnBatteries=false;
        task.Settings.ExecutionTimeLimit="PT0S";
        dynamic action=task.Actions.Create(0);
        action.Path=launch.worker_exe; action.Arguments=state.worker_arguments; action.WorkingDirectory=root;
        dynamic folder=scheduler.GetFolder("\\");
        dynamic registered=folder.RegisterTaskDefinition(state.task,task,2,null,null,3,null);
        registered.RunEx(null,4,checked((int)state.child),null);
    }
    void HideAndRestore() {
        uint foregroundPid; Native.GetWindowThreadProcessId(Native.GetForegroundWindow(),out foregroundPid);
        bool mayRestore=foregroundPid==state.owner.pid;
        Hide();
        uint targetPid; Native.GetWindowThreadProcessId(previousForeground,out targetPid);
        if (mayRestore && previousPid==targetPid && Native.IsWindowVisible(previousForeground)) Native.SetForegroundWindow(previousForeground);
    }
    void Fail(Exception error) { state.error=error.ToString(); CloseOwned(); }
    void CloseOwned() { if (ending) return; stop.Set(); ending=true; Close(); }
    void CloseRequested(object sender,FormClosingEventArgs args) {
        if (!ending && state.phase=="ready") { args.Cancel=true; HideAndRestore(); Save("ready"); return; }
        stop.Set(); ending=true; timer.Stop();
        if (!loginComplete && state.error==null) state.error="Desktop startup was cancelled";
        try {
            Save("stopping");
            Cleanup(root,state);
            state.clean=false; Save("finalizing");
        } catch (Exception error) { state.error=(state.error==null ? "" : state.error+Environment.NewLine)+error; state.clean=false; Save("cleanup_failed"); }
        finally {
            foreach (var handler in events) {
                try { ComEventsHelper.Remove(host.Client,typeof(IMsTscAxEvents).GUID,handler.Key,handler.Value); } catch (COMException) {}
            }
            events.Clear();
            try { if (client!=null && client.Connected==1) client.Disconnect(); } catch (COMException) {}
            stop.Dispose(); show.Dispose(); hide.Dispose(); Native.CloseHandle(parentHandle);
        }
    }
    internal static void Cleanup(string root,Journal state) {
        string workerFailure=null;
        if (state.worker!=null) {
            DateTime deadline=DateTime.UtcNow.AddSeconds(5);
            while (state.worker.Live() && DateTime.UtcNow<deadline) Thread.Sleep(25);
            string receiptPath=Path.Combine(root,"receipt.json");
            if (File.Exists(receiptPath)) {
                var receipt=Files.Read<Dictionary<string,object>>(root,"receipt.json");
                if (!receipt.ContainsKey("version") || Convert.ToInt32(receipt["version"])!=1 ||
                    !receipt.ContainsKey("instance") || (string)receipt["instance"]!=state.instance ||
                    !receipt.ContainsKey("clean") || !(bool)receipt["clean"])
                    workerFailure="Application cleanup was not confirmed by the owned worker";
            } else workerFailure="The owned worker exited or timed out without a cleanup receipt";
        }
        if (state.child!=UInt32.MaxValue && Native.Sessions().Contains(state.child) && Native.Logon(state.child)==state.child_logon) {
            if (state.child==Native.WTSGetActiveConsoleSessionId() || state.child==(uint)Process.GetCurrentProcess().SessionId)
                throw new InvalidOperationException("Recorded child is now a user console or caller desktop; refusing logoff");
            if (!Native.WTSLogoffSession(IntPtr.Zero,state.child,false)) throw Native.Error("Log off owned child");
            DateTime deadline=DateTime.UtcNow.AddSeconds(20);
            while (Native.Sessions().Contains(state.child)) {
                if (DateTime.UtcNow>deadline) throw new TimeoutException("Owned child has not finished logging off");
                Thread.Sleep(50);
            }
        }
        state.child_gone=true;
        if (state.worker!=null && state.worker.Live()) throw new InvalidOperationException("Recorded worker remains alive after child teardown");
        RemoveTask(root,state);
        if (workerFailure!=null) throw new InvalidOperationException(workerFailure);
        if (state.changed_child_setting) {
            bool enabled;
            if (!Native.WTSIsChildSessionsEnabled(out enabled)) throw Native.Error("Read child setting during restoration");
            if (enabled) {
                uint current=Native.Child();
                if (current!=UInt32.MaxValue || !Native.OnlyParentLogon((uint)Process.GetCurrentProcess().SessionId))
                    throw new InvalidOperationException("Another session may use child functionality; the enabled setting was retained");
                if (!Native.WTSEnableChildSessions(false)) throw Native.Error("Restore child-session setting");
            }
            if (!Native.WTSIsChildSessionsEnabled(out enabled) || enabled) throw new InvalidOperationException("Restored child setting did not read back");
        }
    }
    static void RemoveTask(string root,Journal state) {
        if (state.worker_arguments==null) return;
        dynamic scheduler=Activator.CreateInstance(Type.GetTypeFromProgID("Schedule.Service",true)); scheduler.Connect();
        dynamic folder=scheduler.GetFolder("\\"),task;
        try { task=folder.GetTask(state.task); }
        catch (FileNotFoundException) { return; }
        catch (COMException error) { if ((uint)error.ErrorCode==0x80070002) return; throw; }
        dynamic definition=task.Definition;
        string principal=definition.Principal.UserId;
        var sid=principal.StartsWith("S-1-") ? new SecurityIdentifier(principal) : (SecurityIdentifier)new NTAccount(principal).Translate(typeof(SecurityIdentifier));
        if (definition.Actions.Count!=1 || sid.Value!=state.parent.sid || definition.Principal.RunLevel!=0 || definition.Principal.LogonType!=3)
            throw new InvalidOperationException("Owned task principal changed");
        dynamic action=definition.Actions.Item(1);
        if (!String.Equals((string)action.Path,state.parent.image,StringComparison.OrdinalIgnoreCase) ||
            (string)action.Arguments!=state.worker_arguments || !String.Equals((string)action.WorkingDirectory,root,StringComparison.OrdinalIgnoreCase))
            throw new InvalidOperationException("Owned task definition changed");
        DateTime deadline=DateTime.UtcNow.AddSeconds(2);
        while (task.State==4 && DateTime.UtcNow<deadline) Thread.Sleep(50);
        if (task.State==4) throw new InvalidOperationException("Owned worker task is still running");
        folder.DeleteTask(state.task,0);
    }
}

internal static class Program {
    [STAThread] static int Main(string[] args) {
        if (args.Length<1 || args.Length>2) return 2;
        string root=Path.GetFullPath(args[0]); Mutex gate=null; bool held=false;
        try {
            Files.RequirePrivate(root);
            bool created;
            var security=new MutexSecurity();
            security.AddAccessRule(new MutexAccessRule(new SecurityIdentifier(WellKnownSidType.WorldSid,null),MutexRights.Synchronize|MutexRights.Modify,AccessControlType.Allow));
            security.AddAccessRule(new MutexAccessRule(WindowsIdentity.GetCurrent().User,MutexRights.FullControl,AccessControlType.Allow));
            gate=new Mutex(false,"Global\\Glass.Desktop.Owner.v1",out created,security);
            try { held=gate.WaitOne(0); } catch (AbandonedMutexException) { held=true; }
            if (!held) throw new InvalidOperationException("Another Windows desktop owner is active");
            if (args.Length==2 && args[1]=="recover") {
                var state=Files.Read<Journal>(root,"owner.json");
                if (state.version!=1 || state.parent==null || state.owner==null || state.parent.sid!=WindowsIdentity.GetCurrent().User.Value || state.owner.Live())
                    throw new InvalidOperationException("Recovery does not match an exited owner");
                try { DesktopHost.Cleanup(root,state); }
                catch (Exception error) {
                    state.clean=false; state.phase="cleanup_failed"; state.error=error.ToString();
                    Files.Write(root,state); throw;
                }
                state.clean=true; state.phase="stopped"; Files.Write(root,state); return 0;
            }
            Application.SetUnhandledExceptionMode(UnhandledExceptionMode.ThrowException);
            Application.EnableVisualStyles(); Application.SetCompatibleTextRenderingDefault(false);
            var intent=Files.Read<Launch>(root,"intent.json");
            DateTime authorized=DateTime.UtcNow.AddSeconds(10);
            while (!File.Exists(Path.Combine(root,"launch.json"))) {
                if (!intent.parent.Live() || DateTime.UtcNow>authorized) throw new InvalidOperationException("Desktop launch authorization was not published");
                Thread.Sleep(25);
            }
            var window=new DesktopHost(root,Files.Read<Launch>(root,"launch.json"));
            Application.Run(window);
            window.Dispose();
            var result=Files.Read<Journal>(root,"owner.json");
            if (result.phase=="finalizing") { result.clean=true; result.phase="stopped"; result.viewer_visible=false; Files.Write(root,result); }
            return result.clean ? 0 : 1;
        } catch (Exception error) {
            try { Files.WriteText(root,"host-error.txt",error.ToString()); } catch (IOException) {}
            return 1;
        } finally { if (held) gate.ReleaseMutex(); if (gate!=null) gate.Dispose(); }
    }
}
