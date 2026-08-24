package tech.fixedwidth.glassrolefixture;

import android.app.Activity;
import android.os.Bundle;
import android.webkit.WebView;

/**
 * One screen holding a stock {@link WebView} on the shared fixture page, for reading what the
 * two Android readers publish for web content: whether the page's elements arrive at all, and
 * under which class names. Launched on its own so the main screen's readings do not move:
 * {@code adb shell am start -n tech.fixedwidth.glassrolefixture/.WebActivity}.
 *
 * <p>JavaScript is on because the page's button writes its result with it; nothing else is
 * configured, so what is read is the platform's default.
 */
public class WebActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        WebView web = new WebView(this);
        web.getSettings().setJavaScriptEnabled(true);
        web.setContentDescription("the web view");
        web.loadUrl("file:///android_asset/index.html");
        setContentView(web);
    }
}
