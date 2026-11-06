using System;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.IO;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Forms;

public sealed class TrayStatus {
    public uint version;
    public string instance,phase,detail,endpoint,mode,separate_detail,startup_error,viewer_error;
    public ulong acknowledged;
    public bool busy,ready,closing,can_start,start_with_windows,preparing,separate_available;
    public bool? viewer_visible;
}
public sealed class TrayOptions { public uint version; public string instance; public bool hidden; }
public sealed class OpenRequest { public uint version; public string instance,request; }
public sealed class SetupPreferences { public uint version=1; public string endpoint; }

internal enum SetupPage { Desktop,Prepare,Connect,Operating }

internal sealed class SetupFlow {
    internal string CompletedEndpoint;
    internal bool Editing,ConnectionCopied,StartupDraft;
    bool initialized,prepareChange;
    string copiedEndpoint,choiceMode,previousMode;
    ulong choiceSequence;
    DateTime choiceStarted;

    internal bool Complete(TrayStatus value) {
        return value!=null && !String.IsNullOrEmpty(value.endpoint) && value.endpoint==CompletedEndpoint;
    }
    internal void Observe(TrayStatus value) {
        if(!initialized){StartupDraft=value.mode==null || value.start_with_windows;initialized=true;}
        if(copiedEndpoint!=value.endpoint)ConnectionCopied=false;
        if(choiceSequence!=0 && value.acknowledged>=choiceSequence && !value.busy){
            if(value.mode==choiceMode){
                Editing=false;
                prepareChange=Complete(value) && previousMode!=choiceMode && !value.ready;
            }
            choiceSequence=0;
        }
        if(value.ready)prepareChange=false;
    }
    internal SetupPage Page(TrayStatus value,DateTime now) {
        if(value==null)return SetupPage.Desktop;
        if(choiceSequence!=0 && choiceMode=="shared" && value.mode=="shared" && value.busy &&
            now-choiceStarted>=TimeSpan.FromMilliseconds(400))return SetupPage.Prepare;
        if(Editing || value.mode==null)return SetupPage.Desktop;
        if(Complete(value) && !prepareChange)return SetupPage.Operating;
        return value.ready ? SetupPage.Connect : SetupPage.Prepare;
    }
    internal void Edit(){Editing=true;}
    internal void CancelEdit(){Editing=false;}
    internal void Choose(TrayStatus value,string mode,ulong sequence,DateTime now) {
        Editing=true;choiceMode=mode;previousMode=value.mode;choiceSequence=sequence;choiceStarted=now;
    }
    internal void Copied(string endpoint){copiedEndpoint=endpoint;ConnectionCopied=true;}
    internal void Finish(string endpoint){CompletedEndpoint=endpoint;Editing=false;prepareChange=false;}
}

internal sealed class StepNumber : Control {
    internal int Number;
    internal int State;
    internal StepNumber(int number){Number=number;Size=new Size(24,24);Margin=new Padding(0,0,12,0);AccessibleRole=AccessibleRole.StaticText;}
    protected override void OnPaint(PaintEventArgs args){
        base.OnPaint(args);args.Graphics.SmoothingMode=SmoothingMode.AntiAlias;
        Color accent=SystemInformation.HighContrast ? SystemColors.Highlight : Color.FromArgb(0,95,184);
        Color ink=State==2 ? (SystemInformation.HighContrast ? SystemColors.WindowText : Color.FromArgb(16,124,16)) : State==1 ? accent : SystemColors.GrayText;
        using(var pen=new Pen(ink))args.Graphics.DrawEllipse(pen,1,1,Width-3,Height-3);
        if(State==1)using(var brush=new SolidBrush(accent))args.Graphics.FillEllipse(brush,1,1,Width-3,Height-3);
        TextRenderer.DrawText(args.Graphics,State==2 ? "✓" : Number.ToString(),Font,ClientRectangle,
            State==1 ? SystemColors.HighlightText : ink,TextFormatFlags.HorizontalCenter|TextFormatFlags.VerticalCenter);
    }
}

internal sealed class SetupSection : TableLayoutPanel {
    readonly StepNumber number;
    readonly Label title,summary;
    internal readonly FlowLayoutPanel Body;
    internal SetupSection(int index,string text){
        AutoSize=true;AutoSizeMode=AutoSizeMode.GrowAndShrink;ColumnCount=3;RowCount=2;
        Margin=Padding.Empty;Padding=new Padding(24,14,24,14);
        ColumnStyles.Add(new ColumnStyle(SizeType.Absolute,36));
        ColumnStyles.Add(new ColumnStyle(SizeType.Percent,100));ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
        number=new StepNumber(index);title=new Label {Text=text,AutoSize=true,Font=new Font("Segoe UI",10,FontStyle.Bold),Margin=new Padding(0,2,0,0)};
        summary=new Label {AutoSize=true,ForeColor=SystemColors.GrayText,Margin=new Padding(12,3,0,0),Anchor=AnchorStyles.Right};
        Body=new FlowLayoutPanel {AutoSize=true,AutoSizeMode=AutoSizeMode.GrowAndShrink,FlowDirection=FlowDirection.TopDown,WrapContents=false,Margin=new Padding(0,14,0,0)};
        Controls.Add(number,0,0);Controls.Add(title,1,0);Controls.Add(summary,2,0);
        Controls.Add(Body,1,1);SetColumnSpan(Body,2);
    }
    internal void Display(int state,string detail,bool expanded){
        number.State=state;number.AccessibleName=state==2 ? "Complete" : state==1 ? "Current step" : "Next step";number.Invalidate();
        title.ForeColor=state==0 ? SystemColors.GrayText : SystemColors.WindowText;
        if(summary.Text!=detail)summary.Text=detail;
        Body.Visible=expanded;
    }
    protected override void OnPaint(PaintEventArgs args){base.OnPaint(args);using(var pen=new Pen(SystemColors.ControlLight))args.Graphics.DrawLine(pen,0,0,Width,0);}
}

internal sealed class TrayWindow : Form {
    readonly string root,preferencesRoot;
    readonly Launch launch;
    readonly IntPtr parent;
    readonly Action<string> copyText;
    readonly NotifyIcon icon;
    readonly System.Windows.Forms.Timer timer=new System.Windows.Forms.Timer {Interval=200};
    readonly SetupFlow flow=new SetupFlow();
    readonly TableLayoutPanel shell=new TableLayoutPanel(),header=new TableLayoutPanel();
    readonly FlowLayoutPanel content=Stack(),desktopBody=Stack(),prepareBody=Stack(),connectBody=Stack(),operating=Stack(),agentBody=Stack(),settingsBody=Stack();
    readonly Label heading=TextLabel("Application setup",18,true),notice=TextLabel("",10,false),footer=TextLabel("Closing this window keeps Glass in the system tray.",9,false);
    readonly SetupSection desktop=new SetupSection(1,"Choose a desktop"),preparation=new SetupSection(2,"Prepare the environment"),connection=new SetupSection(3,"Connect the agent");
    readonly RadioButton onDesktop=new RadioButton(),offDesktop=new RadioButton();
    readonly Label desktopHint=TextLabel("",10,false),separateHint=TextLabel("",9,false),prepareTitle=TextLabel("",14,true),prepareDetail=TextLabel("",10,false),prepareContext=TextLabel("",9,false);
    readonly Label connectionMessage=TextLabel("",9,false),setupStartupError=TextLabel("",9,false),operatingStatus=TextLabel("",19,true),operatingMode=TextLabel("",10,false),operatingDetail=TextLabel("",10,false),startupError=TextLabel("",9,false),viewerError=TextLabel("",9,false);
    readonly CheckBox setupStartup=new CheckBox(),savedStartup=new CheckBox();
    readonly TextBox setupEndpoint=new TextBox(),savedEndpoint=new TextBox();
    readonly Button apply=new Button(),start=new Button(),cancel=new Button(),change=new Button(),copy=new Button(),finish=new Button(),alreadyConfigured=new Button(),finishUnchanged=new Button(),toggle=new Button(),operatingStart=new Button(),cancelEdit=new Button(),more=new Button();
    readonly Button agentDisclosure=new Button(),settingsDisclosure=new Button();
    readonly Button editDesktop=new Button();
    readonly ContextMenuStrip windowMenu=new ContextMenuStrip();
    readonly ToolStripMenuItem windowSetup=new ToolStripMenuItem("Application setup…"),menuSetup=new ToolStripMenuItem("Application setup…");
    readonly ToolStripMenuItem windowStop=new ToolStripMenuItem("Stop Environment");
    readonly ToolStripMenuItem menuStart=new ToolStripMenuItem("Start Environment…"),menuStop=new ToolStripMenuItem("Stop Environment"),menuViewer=new ToolStripMenuItem("Show Desktop"),menuCopy=new ToolStripMenuItem("Copy endpoint");
    TrayStatus status;
    ulong sequence,finishSequence;
    string lastOpen,localError;
    bool closing,syncing;
    SetupPage? lastPage;

    internal TrayWindow(string root,Launch launch) : this(root,launch,null,true) {}
    internal TrayWindow(string root,Launch launch,Action<string> copyEndpoint,bool showTrayIcon){
        this.root=root;this.launch=launch;preferencesRoot=Path.GetDirectoryName(root);
        copyText=copyEndpoint??new Action<string>(Clipboard.SetText);
        parent=Native.OpenProcess(0x100000,false,launch.parent.pid);
        if(parent==IntPtr.Zero || !launch.parent.Live())throw new InvalidOperationException("Glass is no longer running");
        Files.RequirePrivate(preferencesRoot);
        try{
            if(File.Exists(Path.Combine(preferencesRoot,"application-setup.json"))){
                var saved=Files.Read<SetupPreferences>(preferencesRoot,"application-setup.json");
                if(saved.version!=1)throw new InvalidOperationException("Saved application setup has an unsupported version");
                flow.CompletedEndpoint=saved.endpoint;
            }
        }catch(Exception error){localError="Saved setup progress could not be read. "+error.Message;}
        Text="Glass — Application setup";Name="GlassDesktopSetup";
        AutoScaleDimensions=new SizeF(96,96);AutoScaleMode=AutoScaleMode.Dpi;
        Font=new Font("Segoe UI",10);BackColor=SystemColors.Window;
        ClientSize=new Size(540,640);MinimumSize=new Size(500,420);StartPosition=FormStartPosition.CenterScreen;
        shell.Dock=DockStyle.Fill;shell.ColumnCount=1;shell.RowCount=3;
        shell.RowStyles.Add(new RowStyle(SizeType.AutoSize));shell.RowStyles.Add(new RowStyle(SizeType.Percent,100));shell.RowStyles.Add(new RowStyle(SizeType.AutoSize));
        header.Dock=DockStyle.Top;header.AutoSize=true;header.ColumnCount=2;header.Padding=new Padding(24,18,24,16);
        header.ColumnStyles.Add(new ColumnStyle(SizeType.Percent,100));header.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
        heading.Name="Setup heading";header.Controls.Add(heading,0,0);
        SetButton(more,"⋯",()=>windowMenu.Show(more,new Point(more.Width-windowMenu.Width,more.Height)),false);more.AccessibleName="More options";more.Width=34;header.Controls.Add(more,1,0);
        notice.ForeColor=ErrorColor();header.Controls.Add(notice,0,1);header.SetColumnSpan(notice,2);
        content.Dock=DockStyle.Fill;content.AutoSize=false;content.AutoScroll=true;content.Padding=Padding.Empty;
        footer.ForeColor=SystemColors.GrayText;footer.Padding=new Padding(24,12,24,12);footer.Margin=Padding.Empty;footer.Dock=DockStyle.Fill;
        shell.Controls.Add(header,0,0);shell.Controls.Add(content,0,1);shell.Controls.Add(footer,0,2);Controls.Add(shell);
        BuildSetup();BuildOperating();
        content.Controls.AddRange(new Control[]{desktop,preparation,connection,operating});
        windowSetup.Click+=(sender,args)=>Edit();windowMenu.Items.Add(windowSetup);
        windowStop.Click+=(sender,args)=>Send("stop");windowMenu.Items.Add(windowStop);
        windowMenu.Items.Add(new ToolStripSeparator());windowMenu.Items.Add("Quit Glass",null,(sender,args)=>Quit());
        var menu=new ContextMenuStrip();menuSetup.Click+=(sender,args)=>{Reveal();Edit();};menu.Items.Add(menuSetup);
        menuCopy.Click+=(sender,args)=>CopyEndpoint(null);menu.Items.Add(menuCopy);menu.Items.Add(new ToolStripSeparator());
        menuStart.Click+=(sender,args)=>Send("start");menuStop.Click+=(sender,args)=>Send("stop");menuViewer.Click+=(sender,args)=>ToggleViewer();
        menu.Items.AddRange(new ToolStripItem[]{menuStart,menuStop,menuViewer,new ToolStripSeparator()});
        menu.Items.Add("Quit Glass",null,(sender,args)=>Quit());
        icon=new NotifyIcon {Icon=SystemIcons.Application,Text="Glass: Application setup",Visible=showTrayIcon,ContextMenuStrip=menu};
        icon.DoubleClick+=(sender,args)=>Reveal();
        FormClosing+=(sender,args)=>{if(!closing){args.Cancel=true;Hide();}};
        Resize+=(sender,args)=>FitContent();
        timer.Tick+=(sender,args)=>Poll();timer.Start();UpdateView();
    }
    static FlowLayoutPanel Stack(){return new FlowLayoutPanel {AutoSize=true,AutoSizeMode=AutoSizeMode.GrowAndShrink,FlowDirection=FlowDirection.TopDown,WrapContents=false,Margin=Padding.Empty,Padding=Padding.Empty};}
    static Label TextLabel(string text,int size,bool bold){return new Label {Text=text,AutoSize=true,Font=new Font("Segoe UI",size,bold ? FontStyle.Bold : FontStyle.Regular),Margin=new Padding(0,0,0,10)};}
    static Color ErrorColor(){return SystemInformation.HighContrast ? SystemColors.WindowText : Color.FromArgb(164,38,44);}
    static void SetButton(Button button,string text,Action action,bool primary){
        button.Text=text;button.Name=text;button.AutoSize=true;button.MinimumSize=new Size(0,34);button.Padding=new Padding(10,2,10,2);button.Margin=new Padding(0,8,0,0);
        if(primary){button.FlatStyle=FlatStyle.Flat;button.FlatAppearance.BorderSize=0;button.BackColor=SystemInformation.HighContrast ? SystemColors.Highlight : Color.FromArgb(0,95,184);button.ForeColor=SystemColors.HighlightText;}
        button.Click+=(sender,args)=>action();
    }
    static void SetLink(Button button,string text,Action action){
        SetButton(button,text,action,false);button.FlatStyle=FlatStyle.Flat;button.FlatAppearance.BorderSize=0;button.ForeColor=SystemInformation.HighContrast ? SystemColors.HotTrack : Color.FromArgb(0,95,184);button.Padding=Padding.Empty;
    }
    static TableLayoutPanel Actions(Control left,Control right){
        var row=new TableLayoutPanel {AutoSize=true,AutoSizeMode=AutoSizeMode.GrowAndShrink,ColumnCount=2,Margin=new Padding(0,6,0,0)};
        row.ColumnStyles.Add(new ColumnStyle(SizeType.Percent,100));row.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
        if(left!=null)row.Controls.Add(left,0,0);right.Anchor=AnchorStyles.Right;row.Controls.Add(right,1,0);return row;
    }
    void BuildSetup(){
        desktopBody.Controls.Add(TextLabel("Where should Glass run applications?",14,true));
        onDesktop.Text="On your desktop";offDesktop.Text="Off your desktop…";
        foreach(var radio in new[]{onDesktop,offDesktop}){radio.AutoSize=true;radio.Margin=new Padding(0,3,0,4);radio.CheckedChanged+=(sender,args)=>{if(!syncing)UpdateView();};}
        onDesktop.Checked=true;
        var shared=TextLabel("Use visible windows. Glass can move your pointer, change focus and type.",9,false);shared.ForeColor=SystemColors.GrayText;
        separateHint.ForeColor=SystemColors.GrayText;
        desktopBody.Controls.AddRange(new Control[]{onDesktop,shared,offDesktop,separateHint,desktopHint});
        SetButton(apply,"Continue setup",Apply,true);SetLink(cancelEdit,"Cancel changes",()=>{flow.CancelEdit();UpdateView();});
        desktopBody.Controls.Add(Actions(cancelEdit,apply));desktop.Body.Controls.Add(desktopBody);
        prepareDetail.ForeColor=SystemColors.GrayText;prepareContext.ForeColor=SystemColors.GrayText;
        SetLink(change,"Change desktop",Edit);SetButton(start,"Start Environment…",()=>Send("start"),true);SetButton(cancel,"Cancel",()=>Send("stop"),false);
        prepareBody.Controls.AddRange(new Control[]{prepareTitle,prepareDetail,prepareContext,Actions(change,start),cancel});preparation.Body.Controls.Add(prepareBody);
        connectBody.Controls.Add(TextLabel("Connect your agent",14,true));
        connectBody.Controls.Add(TextLabel("Add this URL as an HTTP MCP connection in your agent.",10,false));
        connectBody.Controls.Add(TextLabel("MCP endpoint",9,false));SetupEndpoint(setupEndpoint);connectBody.Controls.Add(setupEndpoint);
        connectionMessage.ForeColor=SystemColors.GrayText;connectBody.Controls.Add(connectionMessage);
        setupStartup.Text="Start with Windows";setupStartup.AutoSize=true;setupStartup.Margin=new Padding(0,16,0,4);
        setupStartup.CheckedChanged+=(sender,args)=>{if(!syncing)flow.StartupDraft=setupStartup.Checked;};
        connectBody.Controls.Add(setupStartup);connectBody.Controls.Add(TextLabel("Open Glass in the system tray when you sign in.",9,false));
        setupStartupError.ForeColor=ErrorColor();connectBody.Controls.Add(setupStartupError);
        SetButton(copy,"Copy endpoint",()=>CopyEndpoint(setupEndpoint),true);SetButton(finish,"Finish setup",FinishSetup,true);
        SetLink(alreadyConfigured,"I’ve already configured my agent",FinishSetup);SetLink(finishUnchanged,"Finish without changing startup",CompleteSetup);
        connectBody.Controls.AddRange(new Control[]{Actions(null,copy),Actions(null,finish),alreadyConfigured,finishUnchanged});connection.Body.Controls.Add(connectBody);
    }
    static void SetupEndpoint(TextBox field){field.ReadOnly=true;field.Name="MCP endpoint";field.Margin=new Padding(0,2,0,8);}
    void BuildOperating(){
        operating.Padding=new Padding(24,4,24,18);
        operatingMode.ForeColor=SystemColors.GrayText;operatingDetail.ForeColor=SystemColors.GrayText;viewerError.ForeColor=ErrorColor();startupError.ForeColor=ErrorColor();
        SetButton(operatingStart,"Start Environment…",()=>Send(status!=null && status.busy ? "stop" : "start"),true);
        SetLink(toggle,"Show Desktop",ToggleViewer);
        operating.Controls.AddRange(new Control[]{operatingStatus,operatingMode,operatingDetail,operatingStart,toggle,viewerError});
        SetLink(agentDisclosure,"▶ Agent connection",()=>{agentBody.Visible=!agentBody.Visible;agentDisclosure.Text=agentBody.Visible ? "▼ Agent connection" : "▶ Agent connection";});agentDisclosure.AccessibleName="Agent connection";
        agentBody.Controls.Add(TextLabel("HTTP MCP endpoint",9,false));SetupEndpoint(savedEndpoint);agentBody.Controls.Add(savedEndpoint);
        var copyAgain=new Button();SetLink(copyAgain,"Copy endpoint",()=>CopyEndpoint(savedEndpoint));agentBody.Controls.Add(copyAgain);agentBody.Visible=false;
        SetLink(settingsDisclosure,"▶ Settings",()=>{settingsBody.Visible=!settingsBody.Visible;settingsDisclosure.Text=settingsBody.Visible ? "▼ Settings" : "▶ Settings";});settingsDisclosure.AccessibleName="Settings";
        savedStartup.Text="Start with Windows";savedStartup.AutoSize=true;
        savedStartup.CheckedChanged+=(sender,args)=>{if(!syncing && Idle() && savedStartup.Checked!=status.start_with_windows)Send("save_startup",null,savedStartup.Checked);};
        settingsBody.Controls.Add(savedStartup);settingsBody.Controls.Add(TextLabel("Open Glass in the system tray when you sign in.",9,false));settingsBody.Controls.Add(startupError);
        SetLink(editDesktop,"Change desktop…",Edit);settingsBody.Controls.Add(editDesktop);settingsBody.Visible=false;
        operating.Controls.AddRange(new Control[]{agentDisclosure,agentBody,settingsDisclosure,settingsBody});
    }
    bool Idle(){return status!=null && sequence==status.acknowledged && !status.busy && !closing;}
    void Reveal(){Show();WindowState=FormWindowState.Normal;Activate();if(Idle())Send("refresh");}
    void Edit(){if(!Idle())return;flow.Edit();syncing=true;offDesktop.Checked=status.mode=="separate";onDesktop.Checked=!offDesktop.Checked;syncing=false;UpdateView();}
    void Quit(){if(MessageBox.Show(this,"Quit Glass and stop its application work?","Quit Glass",MessageBoxButtons.OKCancel,MessageBoxIcon.Question,MessageBoxDefaultButton.Button2)==DialogResult.OK)Send("quit");}
    void Apply(){
        if(!Idle())return;
        string chosen=offDesktop.Checked ? "separate" : "shared";
        if(status.mode!=null && status.mode!=chosen && MessageBox.Show(this,"Changing desktops stops current Glass application work before switching. Windows may ask for approval to restore setup.","Application setup",MessageBoxButtons.OKCancel,MessageBoxIcon.Question,MessageBoxDefaultButton.Button2)!=DialogResult.OK)return;
        if(Send("configure",chosen,null))flow.Choose(status,chosen,sequence,DateTime.UtcNow);
        UpdateView();
    }
    void CopyEndpoint(TextBox field){
        if(status==null || String.IsNullOrEmpty(status.endpoint) || (!status.ready && !flow.Complete(status)))return;
        flow.ConnectionCopied=false;
        try{copyText(status.endpoint);flow.Copied(status.endpoint);UpdateView();}
        catch(Exception error){if(field!=null){field.Focus();field.SelectAll();}localError="The endpoint could not be copied. Select it and press Ctrl+C. "+error.Message;UpdateView();}
    }
    void FinishSetup(){
        if(!Idle() || !status.ready)return;
        if(flow.StartupDraft==status.start_with_windows && status.startup_error==null){CompleteSetup();return;}
        if(Send("save_startup",null,flow.StartupDraft))finishSequence=sequence;
    }
    void CompleteSetup(){
        if(!Idle() || !status.ready)return;
        try{
            Files.RequirePrivate(preferencesRoot);
            if(File.Exists(Path.Combine(preferencesRoot,"application-setup.json")) && Files.Read<SetupPreferences>(preferencesRoot,"application-setup.json").version!=1)throw new InvalidOperationException("Saved setup progress has an unsupported version and was retained");
            Files.WriteText(preferencesRoot,"application-setup.json",new JavaScriptSerializer().Serialize(new SetupPreferences {endpoint=status.endpoint}));
        }catch(Exception error){localError="Glass is Ready, but setup progress could not be saved. "+error.Message;}
        flow.Finish(status.endpoint);finishSequence=0;UpdateView();
    }
    void ToggleViewer(){if(Idle() && status.ready && status.viewer_visible.HasValue)Send(status.viewer_visible.Value ? "hide" : "show");}
    bool Send(string action){return Send(action,null,null);}
    bool Send(string action,string mode,bool? startup){
        try{
            if(status==null || sequence!=status.acknowledged || closing)return false;
            if(sequence==UInt64.MaxValue)throw new InvalidOperationException("Quit and reopen Glass before sending more operations");
            Files.RequirePrivate(root);
            var request=new {version=1,instance=launch.instance,sequence=sequence+1,action=action,mode=mode,start_with_windows=startup};
            Files.WriteText(root,"request.json",new JavaScriptSerializer().Serialize(request));sequence++;localError=null;UpdateView();return true;
        }catch(Exception error){localError=error.Message;UpdateView();return false;}
    }
    void Finish(){closing=true;Close();Application.ExitThread();}
    void Poll(){
        try{
            if(Native.WaitForSingleObject(parent,0)!=258){Finish();return;}
            if(File.Exists(Path.Combine(root,"open.json"))){
                var open=Files.Read<OpenRequest>(root,"open.json");
                if(open.version!=1 || open.instance!=launch.instance)throw new InvalidOperationException("Application setup belongs to a different launch");
                if(open.request!=lastOpen){lastOpen=open.request;Reveal();}
            }
            if(!File.Exists(Path.Combine(root,"status.json")))return;
            Files.RequirePrivate(root);var next=Files.Read<TrayStatus>(root,"status.json");
            if(next.version!=1 || next.instance!=launch.instance)throw new InvalidOperationException("Application setup belongs to a different launch");
            status=next;if(status.closing){Finish();return;}
            flow.Observe(status);
            if(finishSequence!=0 && status.acknowledged==finishSequence && !status.busy){
                finishSequence=0;if(status.startup_error==null && status.ready)CompleteSetup();
            }
            UpdateView();
        }catch(Exception error){localError=error.Message;UpdateView();}
    }
    void UpdateView(){
        if(icon==null)return;
        SuspendLayout();content.SuspendLayout();
        bool idle=Idle(),ready=status!=null && status.ready,separate=status!=null && status.mode=="separate";
        var page=flow.Page(status,DateTime.UtcNow);
        bool choosing=page==SetupPage.Desktop,preparing=page==SetupPage.Prepare,connecting=page==SetupPage.Connect,daily=page==SetupPage.Operating;
        bool done=flow.Complete(status);
        editDesktop.Enabled=windowSetup.Enabled=menuSetup.Enabled=idle;
        heading.Text=daily ? "Glass" : "Application setup";Text=daily ? "Glass" : "Glass — Application setup";
        string error=localError;
        if(String.IsNullOrEmpty(error) && status!=null && !preparing && status.phase=="Needs attention" && status.detail!=status.startup_error)error=status.detail;
        notice.Text=error??"";notice.Visible=!String.IsNullOrEmpty(error);
        desktop.Visible=preparation.Visible=connection.Visible=!daily;operating.Visible=daily;
        desktop.Display(choosing ? 1 : 2,!choosing && status!=null ? ModeName(status.mode) : "",choosing);
        preparation.Display(preparing ? 1 : ready || done ? 2 : 0,ready ? "Ready" : choosing ? "Next" : "",preparing);
        connection.Display(connecting ? 1 : done ? 2 : 0,connecting ? "" : done ? "Endpoint retained" : "After preparation",connecting);
        onDesktop.Enabled=idle;offDesktop.Enabled=idle && status.separate_available;
        apply.Enabled=idle && (onDesktop.Checked || (offDesktop.Checked && offDesktop.Enabled));
        cancelEdit.Visible=choosing && done;cancelEdit.Enabled=idle;
        desktopHint.Text=status!=null && status.busy ? status.detail : "";desktopHint.Visible=!String.IsNullOrEmpty(desktopHint.Text);
        separateHint.Text=status==null ? "Checking availability…" : status.separate_available ? "Use a separate Windows desktop with your Windows account. Requires setup and sign-in." : status.separate_detail??"Off your desktop is unavailable. Choose On your desktop.";
        if(status!=null){
            bool unavailable=separate && !status.separate_available;
            prepareTitle.Text=unavailable ? "Off your desktop is unavailable" : status.busy ? (status.preparing ? "Complete Windows sign-in" : "Checking your desktop") : separate ? "Start the separate desktop" : "Check your desktop";
            prepareDetail.Text=unavailable ? status.separate_detail : status.busy || status.phase=="Needs attention" ? status.detail : separate ? "Windows may ask for administrator approval and your Windows sign-in." : "Glass will verify that your desktop is available for application work.";
            prepareContext.Text=separate ? "This uses your Windows account. Application containment is a separate setting." : "No additional Windows sign-in is needed.";
            prepareDetail.ForeColor=status.phase=="Needs attention" && !status.busy ? ErrorColor() : SystemColors.GrayText;
            change.Enabled=idle;start.Visible=!status.busy;start.Enabled=idle && status.can_start && !unavailable;
            start.Text=separate ? "Start Environment…" : "Retry checks";cancel.Visible=status.busy;cancel.Enabled=sequence==status.acknowledged;
            if(setupEndpoint.Text!=status.endpoint)setupEndpoint.Text=status.endpoint;
            if(savedEndpoint.Text!=status.endpoint)savedEndpoint.Text=status.endpoint;
            connectionMessage.Text=flow.ConnectionCopied ? "Endpoint copied. Save it in your agent configuration, then finish setup." : "";
            syncing=true;setupStartup.Checked=flow.StartupDraft;savedStartup.Checked=status.start_with_windows;syncing=false;
            setupStartup.Enabled=savedStartup.Enabled=idle;
            setupStartupError.Text=startupError.Text=status.startup_error??"";setupStartupError.Visible=startupError.Visible=!String.IsNullOrEmpty(status.startup_error);
            copy.Visible=!flow.ConnectionCopied;finish.Visible=flow.ConnectionCopied;alreadyConfigured.Visible=!flow.ConnectionCopied;
            copy.Enabled=finish.Enabled=alreadyConfigured.Enabled=idle && ready;
            finishUnchanged.Visible=!String.IsNullOrEmpty(status.startup_error);finishUnchanged.Enabled=idle && ready;
            operatingStatus.Text=ready ? "● Ready" : status.busy ? status.phase : separate && !status.separate_available ? "Environment unavailable" : "Environment stopped";
            operatingStatus.ForeColor=ready && !SystemInformation.HighContrast ? Color.FromArgb(16,124,16) : SystemColors.WindowText;
            operatingMode.Text=ModeName(status.mode);
            operatingDetail.Text=ready ? (separate ? "The separate desktop is ready for application work." : "Glass can move your pointer, change focus and type on your desktop.") : status.detail;
            operatingDetail.ForeColor=status.phase=="Needs attention" ? ErrorColor() : SystemColors.GrayText;
            operatingStart.Visible=!ready;operatingStart.Text=status.busy ? "Cancel" : separate ? "Start Environment…" : "Retry checks";
            operatingStart.Enabled=status.busy ? sequence==status.acknowledged : idle && status.can_start;
            toggle.Visible=separate && ready;toggle.Enabled=idle && ready && status.viewer_visible.HasValue;
            toggle.Text=menuViewer.Text=status.viewer_visible==true ? "Hide Desktop" : "Show Desktop";
            viewerError.Text=status.viewer_error??"";viewerError.Visible=separate && ready && !String.IsNullOrEmpty(status.viewer_error);
            windowStop.Visible=menuStart.Visible=menuStop.Visible=menuViewer.Visible=separate;
            windowStop.Enabled=menuStop.Enabled=sequence==status.acknowledged && (ready || status.busy || status.phase=="Needs attention");
            windowStop.Text=menuStop.Text=status.preparing ? "Cancel" : "Stop Environment";
            menuStart.Enabled=idle && !ready && status.can_start;menuViewer.Enabled=toggle.Enabled;
            menuCopy.Visible=ready || done;menuCopy.Enabled=!String.IsNullOrEmpty(status.endpoint);
            icon.Text="Glass: "+(ready ? "Ready" : status.phase);
        }
        if(!lastPage.HasValue || (lastPage==SetupPage.Operating)!=daily){
            using(var graphics=CreateGraphics()){float scale=graphics.DpiX/96f;ClientSize=new Size((int)(540*scale),(int)((daily ? 430 : 640)*scale));}
        }
        lastPage=page;FitContent();content.ResumeLayout(true);ResumeLayout(true);
    }
    static string ModeName(string mode){return mode=="separate" ? "Off your desktop" : mode=="shared" ? "On your desktop" : "Choose a desktop to continue";}
    void FitContent(){
        int width=Math.Max(300,content.ClientSize.Width-SystemInformation.VerticalScrollBarWidth);
        foreach(Control item in content.Controls){item.MinimumSize=new Size(width,0);item.MaximumSize=new Size(width,0);item.Width=width;}
        int body=Math.Max(200,width-desktop.Padding.Horizontal-(int)desktop.ColumnStyles[0].Width);
        foreach(var stack in new[]{desktopBody,prepareBody,connectBody})FitStack(stack,body);
        FitStack(operating,width);FitStack(agentBody,width-operating.Padding.Horizontal);FitStack(settingsBody,width-operating.Padding.Horizontal);
        notice.MaximumSize=new Size(Math.Max(250,width-48),0);footer.MaximumSize=new Size(width,0);
    }
    static void FitStack(FlowLayoutPanel panel,int width){
        panel.MinimumSize=new Size(width,0);panel.MaximumSize=new Size(width,0);panel.Width=width;
        int available=Math.Max(150,width-panel.Padding.Horizontal);
        foreach(Control child in panel.Controls){
            var label=child as Label;if(label!=null)label.MaximumSize=new Size(available,0);
            else if(child is TableLayoutPanel){child.MinimumSize=new Size(available,0);child.MaximumSize=new Size(available,0);child.Width=available;}
            else if(child is TextBox)child.Width=available;
        }
    }
    protected override void Dispose(bool disposing){
        if(disposing){timer.Stop();timer.Dispose();icon.Visible=false;icon.Dispose();windowMenu.Dispose();Native.CloseHandle(parent);}base.Dispose(disposing);
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
        using(var window=new TrayWindow(root,launch)){if(!options.hidden)window.Show();Application.Run();}
        return 0;
    }
}
