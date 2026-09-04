package tech.fixedwidth.glassrolefixture;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.Context;
import android.os.Bundle;
import android.os.SystemClock;
import android.text.Editable;
import android.text.InputType;
import android.text.TextWatcher;
import android.view.View;
import android.view.ViewGroup;
import android.view.accessibility.AccessibilityNodeInfo;
import android.widget.ArrayAdapter;
import android.widget.Button;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.GridView;
import android.widget.LinearLayout;
import android.widget.ListView;
import android.widget.NumberPicker;
import android.widget.PopupMenu;
import android.widget.ProgressBar;
import android.widget.Switch;
import android.widget.ScrollView;
import android.widget.TabHost;
import android.widget.TableLayout;
import android.widget.TableRow;
import android.widget.TabWidget;
import android.widget.TextView;
import android.widget.Toolbar;

/**
 * One screen holding the controls whose accessibility vocabulary decides a cell in glass's role
 * matrix (`glass_core::role_support::ROLE_SUPPORT`). Everything here is a stock
 * {@code android.widget} class with no support library, so a {@code uiautomator dump} of this app
 * answers one question per control: what widget class does Android report for it?
 *
 * <p>Where a control exists to look at, that reading is what decides whether its matrix cell is a
 * gap or unreachable — a toolbar reporting {@code android.view.ViewGroup}, a popup menu reporting
 * {@code android.widget.ListView}. This app is how it is watched.
 *
 * <p>The two popups need a tap, so they sit behind buttons: the menu and the dialog each replace
 * the dump's topmost window, and are read from a dump of their own.
 *
 * <p>The screen scrolls and its sizes are in dp rather than raw pixels, so every control is
 * reachable on a short screen too. A control scrolled out of view is missing from the dump
 * entirely, and a missing node reads exactly like a missing token — which is the one mistake
 * this app must not make.
 *
 * <p>{@link TabHost} and {@link TabWidget} are deprecated, and deliberately so here: they are
 * what emits the framework tab tokens the matrix records. Replacing them with the Material tab
 * strip real apps use would change the evidence rather than modernize it.
 */
public class MainActivity extends Activity {
    private TextView semanticStatus;
    private TextView semanticFocusStatus;
    private Button movingSemantic;
    private int semanticSaveCount;
    private int semanticTypeCount;
    private int semanticMoveCount;
    private int duplicateSemanticCount;
    private int semanticFocusClickCount;
    private int semanticMovementGeneration;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);

        // Described and forced visible to accessibility: without a description a bare Toolbar can
        // be pruned from the dump, and an absent node cannot tell an unmapped token from a
        // missing one.
        Toolbar toolbar = new Toolbar(this);
        toolbar.setTitle("Role fixture");
        toolbar.setContentDescription("the toolbar");
        toolbar.setImportantForAccessibility(View.IMPORTANT_FOR_ACCESSIBILITY_YES);
        root.addView(toolbar, matchWrap());

        addSemanticControls(root);

        root.addView(tabs(), matchHeight(dp(130)));
        root.addView(label("list"), matchWrap());
        root.addView(list(), matchHeight(dp(80)));
        root.addView(label("grid"), matchWrap());
        root.addView(grid(), matchHeight(dp(80)));
        root.addView(label("table"), matchWrap());
        root.addView(table(), matchWrap());

        // android.widget.Switch is what the platform's own Settings uses, and what CLASS_TOKENS
        // maps to ToggleButton — the control that keeps that mapping re-readable on device.
        Switch active = new Switch(this);
        active.setChecked(true);
        active.setContentDescription("the switch");
        root.addView(active, wrapWrap());

        NumberPicker picker = new NumberPicker(this);
        picker.setMinValue(1);
        picker.setMaxValue(9);
        root.addView(picker, wrapWrap());

        ProgressBar progress =
                new ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal);
        progress.setProgress(40);
        root.addView(progress, matchWrap());

        root.addView(menuButton(), matchWrap());
        root.addView(dialogButton(), matchWrap());

        // Both readers name this by its visible text and surface the content-description as
        // `description` — the positive control proving the two readers agree on precedence.
        Button described = new Button(this);
        described.setText("Save");
        described.setContentDescription("Save changes");
        root.addView(described, matchWrap());

        // A hint and nothing else: no content description, and no resource name (this fixture has
        // no resources file), so it stays unnamed. Through the on-device service its hint arrives
        // as the description, which makes it the one control that renders desc= with no name.
        EditText hinted = new EditText(this);
        hinted.setHint("Search settings");
        root.addView(hinted, matchWrap());

        ScrollView scroller = new ScrollView(this);
        scroller.addView(root);
        setContentView(scroller);
    }

    /** Controls used by the semantic-action device acceptance suite. */
    private void addSemanticControls(LinearLayout root) {
        semanticStatus = new TextView(this);
        semanticStatus.setContentDescription("Semantic Status");
        updateSemanticStatus();
        root.addView(semanticStatus, matchWrap());

        Button save = new Button(this);
        save.setText("Semantic Save");
        save.setContentDescription("Semantic Save");
        save.setOnClickListener(
                new View.OnClickListener() {
                    @Override
                    public void onClick(View view) {
                        semanticSaveCount++;
                        updateSemanticStatus();
                    }
                });
        root.addView(save, matchWrap());

        Button disabled = new Button(this);
        disabled.setText("Disabled semantic");
        disabled.setContentDescription("Disabled semantic");
        disabled.setEnabled(false);
        root.addView(disabled, matchWrap());

        for (int i = 0; i < 2; i++) {
            Button duplicate = new Button(this);
            duplicate.setText("Duplicate semantic");
            duplicate.setContentDescription("Duplicate semantic");
            duplicate.setOnClickListener(
                    new View.OnClickListener() {
                        @Override
                        public void onClick(View view) {
                            duplicateSemanticCount++;
                            updateSemanticStatus();
                        }
                    });
            root.addView(duplicate, matchWrap());
        }

        movingSemantic = new Button(this);
        movingSemantic.setText("Moving semantic");
        movingSemantic.setContentDescription("Moving semantic");
        movingSemantic.setOnClickListener(
                new View.OnClickListener() {
                    @Override
                    public void onClick(View view) {
                        semanticMoveCount++;
                        updateSemanticStatus();
                    }
                });
        root.addView(movingSemantic, matchWrap());

        Button restart = new Button(this);
        restart.setText("Restart movement");
        restart.setContentDescription("Restart movement");
        restart.setOnClickListener(
                new View.OnClickListener() {
                    @Override
                    public void onClick(View view) {
                        startSemanticMovement();
                    }
                });
        root.addView(restart, matchWrap());

        EditText field = new EditText(this);
        semanticFocusStatus = new TextView(this);
        semanticFocusStatus.setContentDescription("Semantic Focus Status");
        updateSemanticFocusStatus(false, false);
        root.addView(semanticFocusStatus, matchWrap());

        field.setContentDescription("Semantic Field");
        field.setText("seed");
        field.setFocusable(true);
        field.setFocusableInTouchMode(true);
        field.setOnClickListener(
                new View.OnClickListener() {
                    @Override
                    public void onClick(View view) {
                        view.requestFocus();
                    }
                });
        field.setOnFocusChangeListener(
                new View.OnFocusChangeListener() {
                    @Override
                    public void onFocusChange(View view, boolean focused) {
                        updateSemanticFocusStatus(true, focused);
                    }
                });
        field.setAccessibilityDelegate(
                new View.AccessibilityDelegate() {
                    @Override
                    public boolean performAccessibilityAction(
                            View host, int action, Bundle arguments) {
                        if (action == AccessibilityNodeInfo.ACTION_CLICK) {
                            semanticFocusClickCount++;
                            boolean requested = host.requestFocus();
                            updateSemanticFocusStatus(requested, host.hasFocus());
                            return requested;
                        }
                        return super.performAccessibilityAction(host, action, arguments);
                    }
                });
        field.addTextChangedListener(
                new TextWatcher() {
                    @Override
                    public void beforeTextChanged(
                            CharSequence text, int start, int count, int after) {}

                    @Override
                    public void onTextChanged(
                            CharSequence text, int start, int before, int count) {}

                    @Override
                    public void afterTextChanged(Editable editable) {
                        semanticTypeCount++;
                        updateSemanticStatus();
                    }
                });
        root.addView(field, matchWrap());

        EditText password = new EditText(this);
        password.setContentDescription("Semantic Password");
        password.setInputType(
                InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD);
        password.setText("PASSWORD_SENTINEL");
        root.addView(password, matchWrap());

        movingSemantic.post(
                new Runnable() {
                    @Override
                    public void run() {
                        startSemanticMovement();
                    }
                });
    }

    private void startSemanticMovement() {
        final int generation = ++semanticMovementGeneration;
        final long started = SystemClock.uptimeMillis();
        final float distance = dp(120);
        movingSemantic.setTranslationX(0);
        movingSemantic.post(
                new Runnable() {
                    @Override
                    public void run() {
                        if (generation != semanticMovementGeneration) return;
                        long elapsed = SystemClock.uptimeMillis() - started;
                        float fraction = Math.min(1f, elapsed / 300f);
                        movingSemantic.setTranslationX(distance * fraction);
                        if (elapsed < 300) movingSemantic.postDelayed(this, 16);
                    }
                });
    }

    private void updateSemanticStatus() {
        semanticStatus.setText(
                "save="
                        + semanticSaveCount
                        + " type="
                        + semanticTypeCount
                        + " move="
                        + semanticMoveCount
                        + " duplicate="
                        + duplicateSemanticCount);
    }

    private void updateSemanticFocusStatus(boolean requested, boolean focused) {
        semanticFocusStatus.setText(
                "focus_click="
                        + semanticFocusClickCount
                        + " request="
                        + requested
                        + " focused="
                        + focused);
    }

    /** A framework {@link PopupMenu} with two entries, opened by tapping the button. */
    private View menuButton() {
        Button button = new Button(this);
        button.setText("popup menu");
        button.setOnClickListener(
                new View.OnClickListener() {
                    @Override
                    public void onClick(View view) {
                        PopupMenu menu = new PopupMenu(MainActivity.this, view);
                        menu.getMenu().add("Alpha");
                        menu.getMenu().add("Beta");
                        menu.show();
                    }
                });
        return button;
    }

    /** A framework {@link AlertDialog} with a title, a message and two buttons. */
    private View dialogButton() {
        Button button = new Button(this);
        button.setText("alert dialog");
        button.setOnClickListener(
                new View.OnClickListener() {
                    @Override
                    public void onClick(View view) {
                        new AlertDialog.Builder(MainActivity.this)
                                .setTitle("Dialog title")
                                .setMessage("Dialog message")
                                .setPositiveButton("OK", null)
                                .setNegativeButton("Cancel", null)
                                .show();
                    }
                });
        return button;
    }

    /**
     * A {@link TabHost} built by hand, since the framework widget needs its three well-known ids
     * ({@code tabhost}, {@code tabs}, {@code tabcontent}) to wire itself up.
     */
    private View tabs() {
        final Context context = this;
        TabHost host = new TabHost(this, null);
        host.setId(android.R.id.tabhost);

        LinearLayout holder = new LinearLayout(this);
        holder.setOrientation(LinearLayout.VERTICAL);
        TabWidget widget = new TabWidget(this);
        widget.setId(android.R.id.tabs);
        FrameLayout content = new FrameLayout(this);
        content.setId(android.R.id.tabcontent);
        holder.addView(widget, matchWrap());
        holder.addView(content, matchHeight(dp(90)));
        host.addView(holder, matchWrap());
        host.setup();

        for (final String name : new String[] {"First", "Second"}) {
            TabHost.TabSpec spec = host.newTabSpec(name);
            spec.setIndicator(name);
            spec.setContent(
                    new TabHost.TabContentFactory() {
                        @Override
                        public View createTabContent(String tag) {
                            TextView view = new TextView(context);
                            view.setText(name + " tab body");
                            return view;
                        }
                    });
            host.addTab(spec);
        }
        return host;
    }

    private View list() {
        ListView view = new ListView(this);
        view.setAdapter(
                new ArrayAdapter<String>(
                        this,
                        android.R.layout.simple_list_item_1,
                        new String[] {"Row one", "Row two"}));
        return view;
    }

    private View grid() {
        GridView view = new GridView(this);
        view.setNumColumns(2);
        view.setAdapter(
                new ArrayAdapter<String>(
                        this,
                        android.R.layout.simple_list_item_1,
                        new String[] {"Cell A", "Cell B", "Cell C", "Cell D"}));
        return view;
    }

    private View table() {
        TableLayout table = new TableLayout(this);
        for (int row = 0; row < 2; row++) {
            TableRow tableRow = new TableRow(this);
            for (int column = 0; column < 2; column++) {
                TextView cell = new TextView(this);
                cell.setText("r" + row + "c" + column);
                cell.setPadding(24, 12, 24, 12);
                tableRow.addView(cell);
            }
            table.addView(tableRow);
        }
        return table;
    }

    private View label(String text) {
        TextView view = new TextView(this);
        view.setText(text);
        return view;
    }

    private LinearLayout.LayoutParams matchWrap() {
        return new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
    }

    private LinearLayout.LayoutParams wrapWrap() {
        return new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
    }

    private LinearLayout.LayoutParams matchHeight(int height) {
        return new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, height);
    }

    /** Density-independent pixels to device pixels, so a height means the same on any screen. */
    private int dp(int value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }
}
