package tech.fixedwidth.glassrolefixture;

import android.app.Activity;
import android.graphics.PixelFormat;
import android.os.Bundle;
import android.view.Gravity;
import android.view.MotionEvent;
import android.view.TouchDelegate;
import android.view.View;
import android.view.WindowManager;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.TextView;

/** Overlapping controls and a separate window, with independent tap counters. */
public class OcclusionActivity extends Activity {
    private int targetHits;
    private int coverHits;
    private int positiveHits;
    private TextView counters;
    private View cover;
    private boolean windowCover;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        String mode = getIntent().getStringExtra("mode");
        if (mode == null) {
            String component = getIntent().getComponent().getShortClassName();
            switch (component) {
                case ".OcclusionWindow": mode = "window"; break;
                case ".OcclusionWindowNarrow": mode = "window-narrow"; break;
                case ".OcclusionWindowPass": mode = "window-pass"; break;
                default: mode = "same";
            }
        }
        final String scenario = mode;
        FrameLayout root = new FrameLayout(this);
        counters = new TextView(this);
        counters.setContentDescription("Occlusion counters");
        root.addView(counters, at(30, 30, 340, 60));
        Button target = button("Covered action", () -> { targetHits++; update(); });
        root.addView(target, at(40, 160, 240, 64));
        Button positive = button("Positive action", () -> {
            positiveHits++;
            removeCover();
            update();
        });
        root.addView(positive, at(40, 280, 240, 64));
        setContentView(root);
        update();

        if (mode.equals("none")) return;
        Button covering = new Button(this) {
            @Override public boolean onTouchEvent(MotionEvent event) {
                if (scenario.equals("view-pass")) return false;
                return super.onTouchEvent(event);
            }
        };
        covering.setText("Cover action");
        covering.setContentDescription("Cover action");
        covering.setOnClickListener(view -> { coverHits++; update(); });
        cover = covering;
        if (mode.equals("hidden")) {
            covering.setImportantForAccessibility(View.IMPORTANT_FOR_ACCESSIBILITY_NO_HIDE_DESCENDANTS);
        }
        if (mode.equals("delegate")) {
            root.addView(covering, at(40, 400, 240, 64));
            root.post(() -> root.setTouchDelegate(new TouchDelegate(
                    new android.graphics.Rect(dp(40), dp(160), dp(280), dp(224)), covering)));
        } else if (mode.startsWith("window")) {
            windowCover = true;
            root.post(() -> {
                int[] origin = new int[2];
                target.getLocationOnScreen(origin);
                boolean narrow = scenario.equals("window-narrow");
                WindowManager.LayoutParams params = new WindowManager.LayoutParams(
                        dp(narrow ? 80 : 240), dp(64),
                        WindowManager.LayoutParams.TYPE_APPLICATION_PANEL,
                        WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE
                                | WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL
                                | WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
                        PixelFormat.TRANSLUCENT);
                if (scenario.equals("window-pass")) {
                    params.flags |= WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE;
                }
                params.gravity = Gravity.TOP | Gravity.LEFT;
                params.x = origin[0] + dp(narrow ? 80 : 0);
                params.y = origin[1];
                params.token = root.getWindowToken();
                params.setTitle("Glass occlusion cover");
                getWindowManager().addView(covering, params);
            });
        } else {
            boolean narrow = mode.equals("narrow");
            root.addView(covering, at(narrow ? 120 : 40, 160, narrow ? 80 : 240, 64));
        }
    }

    private int dp(int value) { return Math.round(value * getResources().getDisplayMetrics().density); }

    private FrameLayout.LayoutParams at(int x, int y, int w, int h) {
        FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(dp(w), dp(h));
        params.leftMargin = dp(x);
        params.topMargin = dp(y);
        return params;
    }

    private Button button(String name, Runnable action) {
        Button button = new Button(this);
        button.setText(name);
        button.setContentDescription(name);
        button.setOnClickListener(view -> action.run());
        return button;
    }

    private void update() {
        counters.setText("target=" + targetHits + " cover=" + coverHits + " positive=" + positiveHits);
    }

    private void removeCover() {
        if (cover == null) return;
        if (windowCover) getWindowManager().removeView(cover);
        else ((android.view.ViewGroup) cover.getParent()).removeView(cover);
        cover = null;
    }

    @Override protected void onDestroy() {
        removeCover();
        super.onDestroy();
    }
}
