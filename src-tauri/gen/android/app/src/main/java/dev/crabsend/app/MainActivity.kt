package dev.crabsend.app

import android.content.ActivityNotFoundException
import android.content.ContentUris
import android.content.Intent
import android.media.MediaScannerConnection
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.DocumentsContract
import android.provider.MediaStore
import android.provider.Settings
import android.system.Os
import android.util.Log
import android.view.View
import android.webkit.MimeTypeMap
import android.widget.Toast
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.Keep
import androidx.core.content.FileProvider
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import org.json.JSONArray
import org.json.JSONObject
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

  /**
   * Leaves the name this device is known by where the native side looks for it.
   *
   * Android keeps that name in its own settings — `Settings.Global.DEVICE_NAME`,
   * which is what the "Device name" field under Settings → About phone holds, and
   * which falls back to the model the device was built as. Only Java can read it,
   * and the native side needs it while it starts, before there is a webview to
   * call Java from, so it travels as an environment variable.
   */
  private fun publishDeviceName() {
    val chosen =
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N_MR1) {
        Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME)
      } else {
        null
      }
    val name = chosen?.trim().takeUnless { it.isNullOrEmpty() } ?: Build.MODEL
    Os.setenv(DEVICE_NAME_ENV, name, true)
  }

  companion object {
    /**
     * The variable the native side reads this device's name from; see
     * [publishDeviceName]. It has to match `DEVICE_NAME_ENV` in `settings.rs`.
     */
    private const val DEVICE_NAME_ENV = "CRABSEND_DEVICE_NAME"

    /** Where the folder picker opens: the folder received files land in. */
    private val DOWNLOAD: Uri =
      Uri.parse("content://com.android.externalstorage.documents/document/primary%3ADownload")

    /** What a row of a folder listing is read for. */
    private val COLUMNS =
      arrayOf(
        DocumentsContract.Document.COLUMN_DOCUMENT_ID,
        DocumentsContract.Document.COLUMN_DISPLAY_NAME,
        DocumentsContract.Document.COLUMN_MIME_TYPE,
        DocumentsContract.Document.COLUMN_LAST_MODIFIED,
      )

    /** How many files one folder may produce; the native side caps the same way. */
    private const val MAX_ENTRIES = 10_000

    /** How deep the walk follows folders; [`files.rs`] follows as deep. */
    private const val MAX_DEPTH = 32
  }

  /** Where the folder picker's answer goes; see [pickFolder] and [listFolder]. */
  private var folderPicker: ActivityResultLauncher<Uri?>? = null

  /**
   * Opens the system's folder picker for the native side.
   *
   * Only Android's own document picker can hand out a folder, and what it hands
   * out is a tree URI rather than a path: [listFolder] walks it, and the answer
   * travels back through [folderPicked]. Nothing picked answers with an empty
   * string, which is what leaves the file list as it was.
   */
  @Keep
  fun pickFolder() {
    val launcher = folderPicker ?: return folderPicked("")
    try {
      launcher.launch(DOWNLOAD)
    } catch (error: Exception) {
      // A picker that will not accept where it was told to start still opens.
      Log.w("Crabsend", "the folder picker refused $DOWNLOAD: ${error.message}")
      launcher.launch(null)
    }
  }

  /** Called from Rust over JNI when the folder picker is done. */
  private external fun folderPicked(listing: String)

  /**
   * Walks a picked tree and describes the files in it, as the native side's
   * `inspect_listing` reads it:
   * `{"name": "<folder>", "entries": [{"uri": …, "name": "sub/file.txt", …}]}`.
   *
   * A provider answers with document ids rather than paths, and a child is
   * addressed by its parent's id, which is why the walk carries both down. Sizes
   * and times are the provider's own; a provider that knows neither answers with
   * zeros, and the bytes are copied out of it later, by the native side.
   */
  private fun listFolder(tree: Uri): String {
    val entries = JSONArray()
    val root = DocumentsContract.getTreeDocumentId(tree)
    val folder = root.substringAfterLast(':').substringAfterLast('/').ifEmpty { "folder" }
    val pending = ArrayDeque<Triple<String, String, Int>>()
    pending.addLast(Triple(root, "", 0))
    while (pending.isNotEmpty() && entries.length() < MAX_ENTRIES) {
      val (id, prefix, depth) = pending.removeFirst()
      val children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, id)
      contentResolver.query(children, COLUMNS, null, null, null)?.use { rows ->
        while (rows.moveToNext() && entries.length() < MAX_ENTRIES) {
          val child = rows.getString(0) ?: continue
          val display = rows.getString(1) ?: continue
          val path = if (prefix.isEmpty()) display else "$prefix/$display"
          if (rows.getString(2) == DocumentsContract.Document.MIME_TYPE_DIR) {
            if (depth + 1 < MAX_DEPTH) {
              pending.addLast(Triple(child, path, depth + 1))
            }
          } else {
            entries.put(
              JSONObject()
                .put("uri", DocumentsContract.buildDocumentUriUsingTree(tree, child).toString())
                .put("name", path)
                .put("modified", if (rows.isNull(3)) 0L else rows.getLong(3))
            )
          }
        }
      }
    }
    return JSONObject().put("name", folder).put("entries", entries).toString()
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    // Before the runtime starts: what it reads while it starts is this.
    publishDeviceName()
    // Registered before the activity is started, which is when the framework
    // requires it: the folder picker answers here, out of the system's own
    // document interface.
    folderPicker =
      registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { tree ->
        folderPicked(if (tree == null) "" else listFolder(tree))
      }
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
