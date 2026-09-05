package tech.fixedwidth.glassrolefixture;

import android.app.Activity;
import android.os.Bundle;
import android.webkit.JavascriptInterface;
import android.webkit.WebView;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;

/** Native entry, embedded account form, and native review of the saved value. */
public class InteractionActivity extends Activity {
    private String saved = "";
    private int submissions;
    private int reviews;
    private LinearLayout root;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        screen();
        readout("Native stage", "entry");
        action("Open form", () -> openForm());
    }

    private void screen() {
        root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(20, 20, 20, 20);
        setContentView(root);
    }

    private void readout(String name, String value) {
        TextView text = new TextView(this);
        text.setText(name + ": " + value);
        text.setContentDescription(name + ": " + value);
        root.addView(text);
    }

    private void action(String name, Runnable task) {
        Button button = new Button(this);
        button.setText(name);
        button.setOnClickListener(view -> task.run());
        root.addView(button);
    }

    private void openForm() {
        screen();
        readout("Native stage", "form");
        action("Review saved value", () -> {
            reviews++;
            screen();
            readout("Native stage", "review");
            readout("Native saved value", saved);
            readout("Native submission count", Integer.toString(submissions));
            readout("Native review count", Integer.toString(reviews));
        });
        WebView web = new WebView(this);
        web.getSettings().setJavaScriptEnabled(true);
        web.setContentDescription("Account web form");
        web.addJavascriptInterface(new Account(), "accountStore");
        root.addView(web, new LinearLayout.LayoutParams(-1, 0, 1));
        web.loadUrl("file:///android_asset/interaction.html");
    }

    private final class Account {
        @JavascriptInterface
        public void save(String value) {
            runOnUiThread(() -> { saved = value; submissions++; });
        }
    }
}
