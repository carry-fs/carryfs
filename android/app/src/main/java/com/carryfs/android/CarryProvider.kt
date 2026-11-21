package com.carryfs.android

import android.content.res.AssetFileDescriptor
import android.database.Cursor
import android.database.MatrixCursor
import android.graphics.Point
import android.os.CancellationSignal
import android.os.ParcelFileDescriptor
import android.os.ProxyFileDescriptorCallback
import android.os.storage.StorageManager
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.system.ErrnoException
import android.system.OsConstants
import android.util.Log
import android.webkit.MimeTypeMap
import java.io.FileNotFoundException
import java.io.IOException
import kotlin.math.max

/**
 * Shows each mounted CarryFS volume as a root in the system file picker and
 * Files app. Document IDs are "volume/object"; object IDs survive renames
 * and moves, so granted URIs keep working.
 */
class CarryProvider : DocumentsProvider() {
    companion object {
        const val AUTHORITY = "com.carryfs.android.documents"
        private const val TAG = "CarryFS"

        private val ROOT_COLUMNS = arrayOf(
            Root.COLUMN_ROOT_ID, Root.COLUMN_FLAGS, Root.COLUMN_TITLE, Root.COLUMN_SUMMARY,
            Root.COLUMN_DOCUMENT_ID, Root.COLUMN_AVAILABLE_BYTES, Root.COLUMN_ICON,
        )
        private val DOCUMENT_COLUMNS = arrayOf(
            Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE,
            Document.COLUMN_SIZE, Document.COLUMN_LAST_MODIFIED, Document.COLUMN_FLAGS,
        )

        fun documentId(volume: Long, id: Long) = "$volume/$id"

        fun rootUri(volume: Long) = DocumentsContract.buildRootUri(AUTHORITY, volume.toString())
    }

    private class Doc(val volume: Long, val id: Long)

    override fun onCreate() = true

    override fun queryRoots(projection: Array<String>?): Cursor {
        val cursor = MatrixCursor(projection ?: ROOT_COLUMNS)
        for (volume in Drives.volumes()) {
            // A faulted volume still lists, so its files can be tried and its error seen.
            val available = runCatching { Native.space(volume)[0] }.getOrNull()
            val writable = runCatching { !Native.readOnly(volume) }.getOrDefault(false)
            var flags = Root.FLAG_LOCAL_ONLY or Root.FLAG_SUPPORTS_IS_CHILD
            if (writable) flags = flags or Root.FLAG_SUPPORTS_CREATE
            cursor.newRow()
                .add(Root.COLUMN_ROOT_ID, volume.toString())
                .add(Root.COLUMN_FLAGS, flags)
                .add(Root.COLUMN_TITLE, runCatching { Native.label(volume) }.getOrDefault("CarryFS"))
                .add(Root.COLUMN_SUMMARY, context!!.getString(R.string.root_summary))
                .add(Root.COLUMN_DOCUMENT_ID, documentId(volume, Native.ROOT))
                .add(Root.COLUMN_AVAILABLE_BYTES, available)
                .add(Root.COLUMN_ICON, R.drawable.ic_cube)
        }
        cursor.setNotificationUri(context!!.contentResolver, DocumentsContract.buildRootsUri(AUTHORITY))
        return cursor
    }

    override fun queryDocument(documentId: String, projection: Array<String>?): Cursor {
        val doc = parse(documentId)
        val cursor = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        val entry = call { Native.entry(doc.volume, doc.id) }
        addRow(cursor, doc.volume, entry, writable(doc.volume))
        return cursor
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<String>?,
        sortOrder: String?,
    ): Cursor {
        val parent = parse(parentDocumentId)
        val cursor = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        val writable = writable(parent.volume)
        for (entry in call { Native.children(parent.volume, parent.id) }) {
            addRow(cursor, parent.volume, entry, writable)
        }
        cursor.setNotificationUri(context!!.contentResolver, childrenUri(parentDocumentId))
        return cursor
    }

    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean {
        val parent = parse(parentDocumentId)
        val doc = parse(documentId)
        if (parent.volume != doc.volume) return false
        var id = doc.id
        // Bounded in case a damaged volume has a parent cycle.
        repeat(4096) {
            if (id == parent.id) return true
            if (id == Native.ROOT) return false
            id = try {
                Native.entry(doc.volume, id).parent
            } catch (e: IOException) {
                return false
            }
        }
        return false
    }

    override fun createDocument(parentDocumentId: String, mimeType: String, displayName: String): String {
        val parent = parse(parentDocumentId)
        val directory = mimeType == Document.MIME_TYPE_DIR
        var name = displayName
        // Like the platform's providers, give a typed file the extension of its type.
        if (!directory && mimeOf(name) != mimeType) {
            MimeTypeMap.getSingleton().getExtensionFromMimeType(mimeType)?.let { name = "$name.$it" }
        }
        val id = call { Native.create(parent.volume, parent.id, name, directory) }
        notifyChildren(parentDocumentId)
        return documentId(parent.volume, id)
    }

    override fun deleteDocument(documentId: String) {
        val doc = parse(documentId)
        val parent = call { Native.entry(doc.volume, doc.id).parent }
        call { Native.delete(doc.volume, doc.id) }
        notifyChildren(documentId(doc.volume, parent))
    }

    override fun renameDocument(documentId: String, displayName: String): String? {
        val doc = parse(documentId)
        val parent = call { Native.entry(doc.volume, doc.id).parent }
        call { Native.rename(doc.volume, doc.id, displayName) }
        notifyChildren(documentId(doc.volume, parent))
        return null
    }

    override fun moveDocument(
        sourceDocumentId: String,
        sourceParentDocumentId: String,
        targetParentDocumentId: String,
    ): String {
        val doc = parse(sourceDocumentId)
        val from = parse(sourceParentDocumentId)
        val to = parse(targetParentDocumentId)
        if (doc.volume != from.volume || doc.volume != to.volume) {
            throw UnsupportedOperationException("Moving between CarryFS volumes is not supported")
        }
        call { Native.move(doc.volume, doc.id, from.id, to.id) }
        notifyChildren(sourceParentDocumentId)
        notifyChildren(targetParentDocumentId)
        return sourceDocumentId
    }

    override fun openDocumentThumbnail(documentId: String, sizeHint: Point, signal: CancellationSignal?): AssetFileDescriptor {
        val doc = parse(documentId)
        val entry = call { Native.entry(doc.volume, doc.id) }
        val size = max(sizeHint.x, sizeHint.y).coerceIn(96, 1024)
        val preview = try {
            Thumbnails.file(context!!.cacheDir, doc.volume, entry, size)
        } catch (e: IOException) {
            throw FileNotFoundException("No preview for ${entry.name}: ${e.message}")
        }
        return AssetFileDescriptor(ParcelFileDescriptor.open(preview, ParcelFileDescriptor.MODE_READ_ONLY), 0, preview.length())
    }

    override fun openDocument(documentId: String, mode: String, signal: CancellationSignal?): ParcelFileDescriptor {
        val doc = parse(documentId)
        val write = 'w' in mode || 'a' in mode
        // "w" truncates, as ParcelFileDescriptor.parseMode reads it.
        val truncate = 't' in mode || mode == "w"
        val callbacks = Drives.owner(doc.volume)?.callbacks ?: throw FileNotFoundException("Drive disconnected")
        val file = call { Native.open(doc.volume, doc.id, write, truncate) }
        val storage = context!!.getSystemService(StorageManager::class.java)
        try {
            return storage.openProxyFileDescriptor(ParcelFileDescriptor.parseMode(mode), OpenFile(doc.volume, file), callbacks)
        } catch (e: Exception) {
            Native.close(doc.volume, file)
            throw e
        }
    }

    /** An open file served to another app through a proxy descriptor. */
    private class OpenFile(val volume: Long, val file: Long) : ProxyFileDescriptorCallback() {
        override fun onGetSize() = errno("fstat") { Native.size(volume, file) }

        override fun onRead(offset: Long, size: Int, data: ByteArray) =
            errno("read") { Native.read(volume, file, offset, size, data) }

        override fun onWrite(offset: Long, size: Int, data: ByteArray) =
            errno("write") { Native.write(volume, file, offset, size, data); size }

        override fun onFsync() = errno("fsync") { Native.sync(volume) }

        override fun onRelease() {
            try {
                Native.close(volume, file)
            } catch (e: IOException) {
                Log.e(TAG, "Publishing a closed file failed; its recent writes may be lost", e)
            }
        }

        private inline fun <T> errno(operation: String, block: () -> T): T = try {
            block()
        } catch (e: FileNotFoundException) {
            throw ErrnoException(operation, OsConstants.ENOENT)
        } catch (e: IOException) {
            Log.e(TAG, "$operation failed", e)
            throw ErrnoException(operation, OsConstants.EIO)
        }
    }

    private fun addRow(cursor: MatrixCursor, volume: Long, entry: Entry, writable: Boolean) {
        var flags = 0
        if (writable) {
            flags = Document.FLAG_SUPPORTS_DELETE or Document.FLAG_SUPPORTS_RENAME or Document.FLAG_SUPPORTS_MOVE
            flags = flags or if (entry.directory) Document.FLAG_DIR_SUPPORTS_CREATE else Document.FLAG_SUPPORTS_WRITE
        }
        if (!entry.directory && mimeOf(entry.name).startsWith("image/")) flags = flags or Document.FLAG_SUPPORTS_THUMBNAIL
        cursor.newRow()
            .add(Document.COLUMN_DOCUMENT_ID, documentId(volume, entry.id))
            .add(Document.COLUMN_DISPLAY_NAME, entry.name)
            .add(Document.COLUMN_MIME_TYPE, if (entry.directory) Document.MIME_TYPE_DIR else mimeOf(entry.name))
            .add(Document.COLUMN_SIZE, entry.size)
            .add(Document.COLUMN_LAST_MODIFIED, entry.modified)
            .add(Document.COLUMN_FLAGS, flags)
    }

    private fun writable(volume: Long) = !call { Native.readOnly(volume) }

    private fun mimeOf(name: String): String {
        val extension = name.substringAfterLast('.', "").lowercase()
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension) ?: "application/octet-stream"
    }

    private fun parse(documentId: String): Doc {
        val parts = documentId.split('/')
        val volume = parts.getOrNull(0)?.toLongOrNull()
        val id = parts.getOrNull(1)?.toLongOrNull()
        if (parts.size != 2 || volume == null || id == null) throw FileNotFoundException("Unknown document $documentId")
        return Doc(volume, id)
    }

    private fun childrenUri(documentId: String) = DocumentsContract.buildChildDocumentsUri(AUTHORITY, documentId)

    private fun notifyChildren(documentId: String) =
        context!!.contentResolver.notifyChange(childrenUri(documentId), null)

    /**
     * Binder passes only some exception types back to the caller, so an I/O
     * failure becomes IllegalStateException. FileNotFoundException passes as is.
     */
    private inline fun <T> call(block: () -> T): T = try {
        block()
    } catch (e: FileNotFoundException) {
        throw e
    } catch (e: IOException) {
        throw IllegalStateException(e.message, e)
    }
}
