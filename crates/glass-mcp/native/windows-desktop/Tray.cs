using System;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Forms;

public sealed class TrayStatus {
    public uint version;
    public string instance,phase,detail,endpoint,mode,separate_detail;
    public ulong acknowledged;
    public bool busy,ready,closing,can_start,start_with_windows,preparing,separate_available;
}
public sealed class TrayOptions { public uint version; public string instance; public bool hidden; }
public sealed class OpenRequest { public uint version; public string instance,request; }

internal sealed class TrayWindow : Form {
    readonly string root;
    readonly Launch launch;
    readonly IntPtr parent;
    readonly NotifyIcon icon;
    readonly System.Windows.Forms.Timer timer=new System.Windows.Forms.Timer { Interval=200 };
    readonly Label heading=new Label(),currentMode=new Label(),notice=new Label();
    readonly TextBox endpoint=new TextBox();
    readonly RadioButton onDesktop=new RadioButton(),offDesktop=new RadioButton();
    readonly Label separateHint=new Label();
    readonly CheckBox startup=new CheckBox();
    readonly Button apply=new Button(),start=new Button(),stop=new Button(),show=new Button(),hide=new Button();
    readonly ToolStripMenuItem menuStart=new ToolStripMenuItem("Start Environment…"),
        menuStop=new ToolStripMenuItem("Stop Environment"),menuShow=new ToolStripMenuItem("Show Desktop"),
        menuHide=new ToolStripMenuItem("Hide Desktop");
    TrayStatus status;
    ulong sequence;
    string lastOpen;
    bool closing,editing,syncing;

    internal TrayWindow(string root,Launch launch) {
        this.root=root;this.launch=launch;
        parent=Native.OpenProcess(0x100000,false,launch.parent.pid);
        if(parent==IntPtr.Zero || !launch.parent.Live())throw new InvalidOperationException("Glass is no longer running");
        Text="Glass — Application setup";Name="GlassDesktopSetup";
        ClientSize=new Size(620,550);MinimumSize=new Size(630,590);
        AutoScaleMode=AutoScaleMode.Dpi;Font=new Font("Segoe UI",10);
        StartPosition=FormStartPosition.CenterScreen;
        heading.Text="Application setup";heading.Font=new Font(Font.FontFamily,17,FontStyle.Bold);heading.SetBounds(24,18,570,44);
        currentMode.SetBounds(24,65,570,26);
        notice.Text="Choose where Glass should run applications.";notice.SetBounds(24,98,570,58);
        var choices=new GroupBox { Text="Where should applications run?" };choices.SetBounds(24,165,570,165);
        onDesktop.Text="On your desktop";onDesktop.AutoSize=true;onDesktop.SetBounds(16,25,500,26);onDesktop.Checked=true;
        var shared=new Label { Text="Use visible windows. Glass can move your pointer, change focus and type.",AutoSize=false };shared.SetBounds(40,53,510,35);
        offDesktop.Text="Off your desktop…";offDesktop.AutoSize=true;offDesktop.SetBounds(16,92,500,26);
        separateHint.SetBounds(40,120,510,38);
        choices.Controls.AddRange(new Control[]{onDesktop,shared,offDesktop,separateHint});
        startup.Text="Start with Windows";startup.AutoSize=true;startup.SetBounds(24,349,330,26);
        onDesktop.CheckedChanged+=(sender,args)=>{if(!syncing)editing=true;};
        offDesktop.CheckedChanged+=(sender,args)=>{if(!syncing)editing=true;};
        startup.CheckedChanged+=(sender,args)=>{if(!syncing)editing=true;};
        Configure(apply,"Continue setup",426,342,168,Apply);
        var endpointLabel=new Label { Text="MCP endpoint",AutoSize=true };endpointLabel.SetBounds(24,397,130,26);
        endpoint.ReadOnly=true;endpoint.SetBounds(155,391,285,29);
        var copy=new Button();Configure(copy,"Copy endpoint",452,389,142,32,()=>{if(endpoint.Text.Length!=0)Clipboard.SetText(endpoint.Text);});
        Configure(start,"Start Environment…",24,445,184,()=>Send("start"));
        Configure(stop,"Stop Environment",220,445,165,()=>Send("stop"));
        Configure(show,"Show Desktop",24,494,184,()=>Send("show"));
        Configure(hide,"Hide Desktop",220,494,165,()=>Send("hide"));
        var quit=new Button();Configure(quit,"Quit Glass",440,494,154,Quit);
        Controls.AddRange(new Control[]{heading,currentMode,notice,choices,startup,apply,endpointLabel,endpoint,copy,start,stop,show,hide,quit});
        var menu=new ContextMenuStrip();
        menu.Items.Add("Application setup…",null,(sender,args)=>Open());
        menu.Items.Add("Copy endpoint",null,(sender,args)=>{if(endpoint.Text.Length!=0)Clipboard.SetText(endpoint.Text);});
        menu.Items.Add(new ToolStripSeparator());
        menuStart.Click+=(sender,args)=>Send("start");menuStop.Click+=(sender,args)=>Send("stop");
        menuShow.Click+=(sender,args)=>Send("show");menuHide.Click+=(sender,args)=>Send("hide");
        menu.Items.AddRange(new ToolStripItem[]{menuStart,menuStop,menuShow,menuHide,new ToolStripSeparator()});
        menu.Items.Add("Quit Glass",null,(sender,args)=>Quit());
        icon=new NotifyIcon { Icon=SystemIcons.Application,Text="Glass: Application setup",Visible=true,ContextMenuStrip=menu };
        icon.DoubleClick+=(sender,args)=>Open();
        FormClosing+=(sender,args)=>{if(!closing){args.Cancel=true;Hide();}};
        timer.Tick+=(sender,args)=>Poll();timer.Start();UpdateButtons();
    }
    void Configure(Button button,string text,int x,int y,int width,Action action){Configure(button,text,x,y,width,34,action);}
    void Configure(Button button,string text,int x,int y,int width,int height,Action action){button.Text=text;button.SetBounds(x,y,width,height);button.Click+=(sender,args)=>action();}
    void Open(){Show();WindowState=FormWindowState.Normal;Activate();if(status!=null && !status.busy)Send("refresh");}
    void Quit(){
        if(MessageBox.Show(this,"Quit Glass and stop its application work?","Quit Glass",MessageBoxButtons.OKCancel,MessageBoxIcon.Question,MessageBoxDefaultButton.Button2)==DialogResult.OK)Send("quit");
    }
    void Apply(){
        string chosen=offDesktop.Checked ? "separate" : "shared";
        if(status!=null && status.mode!=null && status.mode!=chosen &&
            MessageBox.Show(this,"Changing desktops stops current Glass application work before switching.","Application setup",MessageBoxButtons.OKCancel,MessageBoxIcon.Question,MessageBoxDefaultButton.Button2)!=DialogResult.OK)return;
        Send("configure",chosen,startup.Checked);editing=false;
    }
    void Send(string action){Send(action,null,null);}
    void Send(string action,string mode,bool? startWithWindows){
        try{
            if(status==null || sequence!=status.acknowledged || closing)return;
            if(sequence==UInt64.MaxValue)throw new InvalidOperationException("Quit and reopen Glass before sending more operations");
            Files.RequirePrivate(root);
            var request=new { version=1,instance=launch.instance,sequence=sequence+1,action=action,mode=mode,start_with_windows=startWithWindows };
            Files.WriteText(root,"request.json",new JavaScriptSerializer().Serialize(request));sequence++;UpdateButtons();
        }catch(Exception error){heading.Text="Needs attention";notice.Text=error.Message;}
    }
    void Finish(){closing=true;Close();Application.ExitThread();}
    void Poll(){
        try{
            if(Native.WaitForSingleObject(parent,0)!=258){Finish();return;}
            string openFile=Path.Combine(root,"open.json");
            if(File.Exists(openFile)){
                var request=Files.Read<OpenRequest>(root,"open.json");
                if(request.version!=1 || request.instance!=launch.instance)throw new InvalidOperationException("Application setup belongs to a different launch");
                if(request.request!=lastOpen){lastOpen=request.request;Open();}
            }
            if(!File.Exists(Path.Combine(root,"status.json")))return;
            Files.RequirePrivate(root);
            var next=Files.Read<TrayStatus>(root,"status.json");
            if(next.version!=1 || next.instance!=launch.instance)throw new InvalidOperationException("Application setup belongs to a different launch");
            status=next;if(status.closing){Finish();return;}
            heading.Text=status.ready ? "Ready" : status.phase;
            currentMode.Text=status.mode=="shared" ? "On your desktop" : status.mode=="separate" ? "Off your desktop" : "Choose a desktop to continue";
            notice.Text=status.detail;endpoint.Text=status.endpoint;icon.Text="Glass: "+(status.ready ? "Ready" : status.phase);
            if(!editing && !status.busy && sequence==status.acknowledged){
                syncing=true;offDesktop.Checked=status.mode=="separate";onDesktop.Checked=!offDesktop.Checked;
                startup.Checked=status.mode==null || status.start_with_windows;syncing=false;
            }
            UpdateButtons();
        }catch(Exception error){heading.Text="Needs attention";notice.Text=error.Message;}
    }
    void UpdateButtons(){
        bool idle=status!=null && sequence==status.acknowledged && !status.busy;
        bool separate=status!=null && status.mode=="separate";
        onDesktop.Enabled=startup.Enabled=idle;
        offDesktop.Enabled=idle && status.separate_available;
        apply.Enabled=idle && (onDesktop.Checked || (offDesktop.Checked && offDesktop.Enabled));
        separateHint.Text=status==null ? "Checking availability…" : status.separate_available
            ? "Use a separate Windows desktop with your Windows account. Requires setup and sign-in."
            : status.separate_detail ?? "Off your desktop is unavailable. Choose On your desktop.";
        start.Visible=stop.Visible=show.Visible=hide.Visible=separate;
        menuStart.Visible=menuStop.Visible=menuShow.Visible=menuHide.Visible=separate;
        start.Enabled=menuStart.Enabled=idle && !status.ready && status.can_start;
        stop.Enabled=menuStop.Enabled=status!=null && sequence==status.acknowledged && (status.ready || status.busy || status.phase=="Needs attention");
        stop.Text=menuStop.Text=status!=null && status.preparing ? "Cancel" : "Stop Environment";
        show.Enabled=menuShow.Enabled=hide.Enabled=menuHide.Enabled=idle && status.ready;
    }
    protected override void Dispose(bool disposing){
        if(disposing){timer.Stop();timer.Dispose();icon.Visible=false;icon.Dispose();Native.CloseHandle(parent);}base.Dispose(disposing);
    }
}

internal static class TrayApp {
    internal static int Run(string root){
        var intent=Files.Read<Launch>(root,"intent.json");DateTime deadline=DateTime.UtcNow.AddSeconds(10);
        while(!File.Exists(Path.Combine(root,"launch.json"))){if(!intent.parent.Live() || DateTime.UtcNow>deadline)throw new InvalidOperationException("Glass launch was not authorized");Thread.Sleep(25);}
        var launch=Files.Read<Launch>(root,"launch.json");var current=ProcessRecord.Read((uint)Process.GetCurrentProcess().Id);
        if(launch.version!=1 || launch.instance==null || launch.instance.Length!=32 || launch.parent==null || launch.helper==null ||
            launch.parent.session==0 || launch.parent.session!=current.session || launch.parent.logon!=current.logon || launch.parent.sid!=current.sid ||
            !launch.parent.Live() || !launch.helper.Matches(current) || !launch.parent.Matches(intent.parent) || launch.instance!=intent.instance ||
            !String.Equals(launch.worker_exe,launch.parent.image,StringComparison.OrdinalIgnoreCase))throw new InvalidOperationException("Glass is outside its recorded Windows sign-in");
        var options=Files.Read<TrayOptions>(root,"options.json");
        if(options.version!=1 || options.instance!=launch.instance)throw new InvalidOperationException("Application setup options belong to a different launch");
        Application.EnableVisualStyles();Application.SetCompatibleTextRenderingDefault(false);
        using(var window=new TrayWindow(root,launch)){
            if(!options.hidden)window.Show();
            Application.Run();
        }
        return 0;
    }
}
