package dev.crabsend.app

import android.content.ActivityNotFoundException
import android.content.ContentUris
import android.content.Intent
import android.media.MediaScannerConnection
import android.net.Uri
import android.os.Bundle
import android.provider.MediaStore
import android.util.Log
import android.view.View
import android.webkit.MimeTypeMap
import android.widget.Toast
import androidx.activity.enableEdgeToEdge
import androidx.annotation.Keep
import androidx.core.content.FileProvider
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import java.io.File

class MainActivity : TauriActivity() {
  /**
   * Registers a finished transfer with the media database.
   *
   * A file that an application writes by path is its own in a way that keeps
   * every other application out: a file manager lists the folder without it,
   * and nothing indexes it, so galleries and a USB connection miss it as well.
   * Handing the file to the scanner is what makes it turn up in them.
   *
   * Called from Rust over JNI, which no shrinking rule can see: `@Keep` is what
   * keeps the release build from renaming it away.
   */
  @Keep
  fun publishReceivedFile(path: String) {
    MediaScannerConnection.scanFile(this, arrayOf(path), null) { _, uri ->
      if (uri == null) {
        Log.w("Crabsend", "the media database did not accept $path")
      }
    }
  }

  /**
   * Hands a received file to whatever application the phone opens its type
   * with.
   *
   * A phone has no file manager to reveal a file in, so this is the action the
   * interface offers instead. The media database knows the file — that is what
   * [publishReceivedFile] is for — and its content URI is the one to pass on;
   * for a file it does not know, the file provider this application declares
   * stands in. Either way the receiving application is granted the read it
   * needs, and a file no application claims says so rather than doing nothing.
   */
  @Keep
  fun openReceivedFile(path: String) {
    val file = File(path)
    val published = publishedFile(path)
    val uri =
      published?.first
        ?: FileProvider.getUriForFile(this, "$packageName.fileprovider", file)
    val mime =
      published?.second
        ?: MimeTypeMap.getSingleton().getMimeTypeFromExtension(file.extension.lowercase())
        ?: "*/*"

    try {
      startActivity(
        Intent(Intent.ACTION_VIEW)
          .setDataAndType(uri, mime)
          .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
      )
    } catch (error: ActivityNotFoundException) {
      Log.w("Crabsend", "no application opens $mime: $path")
      Toast.makeText(this, getString(R.string.no_application_for_file), Toast.LENGTH_LONG).show()
    }
  }

  /** The media database's URI and type for a file this application received. */
  private fun publishedFile(path: String): Pair<Uri, String>? {
    val files = MediaStore.Files.getContentUri("external")
    val projection =
      arrayOf(MediaStore.MediaColumns._ID, MediaStore.MediaColumns.MIME_TYPE)
    val selection = "${MediaStore.MediaColumns.DATA} = ?"
    contentResolver.query(files, projection, selection, arrayOf(path), null)?.use { rows ->
      if (!rows.moveToFirst()) {
        return null
      }
      val mime = rows.getString(1)?.takeIf { it.isNotEmpty() } ?: return null
      return ContentUris.withAppendedId(files, rows.getLong(0)) to mime
    }
    return null
  }

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
