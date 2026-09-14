package dev.crabsend.app

import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)

    // Android 15 and later lay every app out edge to edge, so nothing keeps the
    // webview clear of the system bars on its own — without this the interface
    // starts underneath the status bar. The insets are applied as padding on the
    // content view and still handed to the children (the scanning overlay is one
    // of them).
    ViewCompat.setOnApplyWindowInsetsListener(findViewById<View>(android.R.id.content)) {
        view,
        insets ->
      val bars =
        insets.getInsets(
          WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
        )
      view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      insets
    }
  }
}
