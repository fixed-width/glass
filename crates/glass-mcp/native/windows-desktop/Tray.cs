using System;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Forms;

public sealed class TrayStatus {
    public uint version;
    public string instance,phase,detail,endpoint;
    public ulong acknowledged;
    public bool busy,ready,closing,can_start;
}

internal sealed class TrayWindow : Form {
    readonly string root;
    readonly Launch launch;
    readonly IntPtr parent;
    readonly NotifyIcon icon;
    readonly System.Windows.Forms.Timer timer=new System.Windows.Forms.Timer { Interval=200 };
    readonly Label heading=new Label(),notice=new Label();
    readonly TextBox endpoint=new TextBox();
    readonly Button start=new Button(),stop=new Button(),show=new Button(),hide=new Button();
    readonly ToolStripMenuItem menuStart=new ToolStripMenuItem("Start development desktop"),
        menuStop=new ToolStripMenuItem("Stop desktop"),menuShow=new ToolStripMenuItem("Show desktop"),
        menuHide=new ToolStripMenuItem("Hide desktop");
    TrayStatus status;
    ulong sequence;
    bool closing;

    internal TrayWindow(string root,Launch launch) {
        this.root=root; this.launch=launch;
        parent=Native.OpenProcess(0x100000,false,launch.parent.pid);
        if (parent==IntPtr.Zero || !launch.parent.Live()) throw new InvalidOperationException("Glass resident process is unavailable");
        Text="Glass"; Name="GlassDesktopSetup";
        ClientSize=new Size(610,340); MinimumSize=new Size(560,370);
        AutoScaleMode=AutoScaleMode.Dpi; Font=new Font("Segoe UI",10);
        StartPosition=FormStartPosition.CenterScreen;
        heading.Text="Not ready"; heading.Font=new Font(Font.FontFamily,18,FontStyle.Bold);
        heading.SetBounds(24,22,550,42);
        notice.Text="Start the development desktop before agent work.";
        notice.SetBounds(24,76,550,80);
        var account=new Label { Text="Apps run on a separate desktop using your Windows account. Containment is selected for each app.", AutoSize=false };
        account.SetBounds(24,158,550,45);
        var endpointLabel=new Label { Text="MCP endpoint",AutoSize=true };
        endpointLabel.SetBounds(24,215,130,24);
        endpoint.ReadOnly=true; endpoint.SetBounds(155,211,300,28);
        var copy=new Button { Text="Copy endpoint" }; copy.SetBounds(465,210,120,30);
        copy.Click+=(sender,args)=>{ if(endpoint.Text.Length!=0) Clipboard.SetText(endpoint.Text); };
        Configure(start,"Start development desktop",24,262,228,()=>Send("start"));
        Configure(stop,"Stop desktop",264,262,148,()=>Send("stop"));
        Configure(show,"Show desktop",24,303,148,()=>Send("show"));
        Configure(hide,"Hide desktop",184,303,148,()=>Send("hide"));
        var quit=new Button(); Configure(quit,"Quit Glass",437,303,148,Quit);
        ClientSize=new Size(610,365);
        Controls.AddRange(new Control[] { heading,notice,account,endpointLabel,endpoint,copy,start,stop,show,hide,quit });
        var menu=new ContextMenuStrip();
        menu.Items.Add("Open Glass",null,(sender,args)=>Open());
        menu.Items.Add(new ToolStripSeparator());
        menuStart.Click+=(sender,args)=>Send("start"); menuStop.Click+=(sender,args)=>Send("stop");
        menuShow.Click+=(sender,args)=>Send("show"); menuHide.Click+=(sender,args)=>Send("hide");
        menu.Items.AddRange(new ToolStripItem[] { menuStart,menuStop,menuShow,menuHide,new ToolStripSeparator() });
        menu.Items.Add("Quit Glass",null,(sender,args)=>Quit());
        icon=new NotifyIcon { Icon=SystemIcons.Application,Text="Glass: Not ready",Visible=true,ContextMenuStrip=menu };
        icon.DoubleClick+=(sender,args)=>Open();
        FormClosing+=(sender,args)=>{ if(!closing){args.Cancel=true;Hide();} };
        timer.Tick+=(sender,args)=>Poll(); timer.Start(); UpdateButtons();
    }

    void Configure(Button button,string text,int x,int y,int width,Action action) {
        button.Text=text; button.SetBounds(x,y,width,34); button.Click+=(sender,args)=>action();
    }
    void Open() { Show(); WindowState=FormWindowState.Normal; Activate(); }
    void Quit() {
        if(MessageBox.Show(this,"Quit Glass and stop its agent work and development desktop?","Quit Glass",
            MessageBoxButtons.OKCancel,MessageBoxIcon.Question,MessageBoxDefaultButton.Button2)==DialogResult.OK) Send("quit");
    }
    void Send(string action) {
        try {
            if(status==null || sequence!=status.acknowledged || closing) return;
            if(sequence==UInt64.MaxValue) throw new InvalidOperationException("Restart Glass before sending more actions");
            Files.RequirePrivate(root);
            var request=new { version=1,instance=launch.instance,sequence=sequence+1,action=action };
            Files.WriteText(root,"request.json",new JavaScriptSerializer().Serialize(request));
            sequence++; UpdateButtons();
        } catch(Exception error) { heading.Text="Needs attention"; notice.Text=error.Message; }
    }
    void Poll() {
        try {
            if(Native.WaitForSingleObject(parent,0)!=258) { closing=true; Close(); return; }
            if(!File.Exists(Path.Combine(root,"status.json"))) return;
            Files.RequirePrivate(root);
            var next=Files.Read<TrayStatus>(root,"status.json");
            if(next.version!=1 || next.instance!=launch.instance) throw new InvalidOperationException("Glass status belongs to a different launch");
            status=next;
            if(status.closing) { closing=true; Close(); return; }
            heading.Text=status.phase; notice.Text=status.detail; endpoint.Text=status.endpoint;
            icon.Text="Glass: "+status.phase;
            UpdateButtons();
        } catch(Exception error) { heading.Text="Needs attention"; notice.Text=error.Message; }
    }
    void UpdateButtons() {
        bool idle=status!=null && sequence==status.acknowledged && !status.busy;
        bool ready=status!=null && status.ready;
        start.Enabled=menuStart.Enabled=idle && !ready && status.can_start;
        stop.Enabled=menuStop.Enabled=status!=null && sequence==status.acknowledged;
        stop.Text=status!=null && status.busy && !ready ? "Cancel starting" : "Stop desktop";
        show.Enabled=menuShow.Enabled=idle && ready;
        hide.Enabled=menuHide.Enabled=idle && ready;
    }
    protected override void Dispose(bool disposing) {
        if(disposing) { timer.Stop(); timer.Dispose(); icon.Visible=false; icon.Dispose(); Native.CloseHandle(parent); }
        base.Dispose(disposing);
    }
}

internal static class TrayApp {
    internal static int Run(string root) {
        var intent=Files.Read<Launch>(root,"intent.json");
        DateTime deadline=DateTime.UtcNow.AddSeconds(10);
        while(!File.Exists(Path.Combine(root,"launch.json"))) {
            if(!intent.parent.Live() || DateTime.UtcNow>deadline) throw new InvalidOperationException("Glass tray launch was not authorized");
            Thread.Sleep(25);
        }
        var launch=Files.Read<Launch>(root,"launch.json");
        var current=ProcessRecord.Read((uint)Process.GetCurrentProcess().Id);
        if(launch.version!=1 || launch.instance==null || launch.instance.Length!=32 || launch.parent==null || launch.helper==null ||
            launch.parent.session==0 || launch.parent.session!=current.session || launch.parent.logon!=current.logon ||
            launch.parent.sid!=current.sid || !launch.parent.Live() || !launch.helper.Matches(current) ||
            !launch.parent.Matches(intent.parent) || launch.instance!=intent.instance ||
            !String.Equals(launch.worker_exe,launch.parent.image,StringComparison.OrdinalIgnoreCase))
            throw new InvalidOperationException("Glass tray is outside its recorded Windows logon");
        Application.EnableVisualStyles(); Application.SetCompatibleTextRenderingDefault(false);
        using(var window=new TrayWindow(root,launch)) Application.Run(window);
        return 0;
    }
}
