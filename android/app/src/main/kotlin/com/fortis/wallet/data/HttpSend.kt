package com.fortis.wallet.data

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import okhttp3.Call
import okhttp3.Callback
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import java.io.IOException
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/**
 * Run [req] and hand the response to [read] (on the IO dispatcher), closing it
 * after. Unlike a blocking `execute()`, this is **cancellable**: cancelling the
 * calling coroutine (a step's time limit, a stopped service) cancels the OkHttp
 * call at once, rather than leaving it to its socket timeouts. The client's
 * `callTimeout` still bounds a call nobody cancels.
 */
suspend fun <T> OkHttpClient.send(
    req: Request,
    /** Where the body is read; a parameter only so tests can stage a cancellation. */
    io: CoroutineDispatcher = Dispatchers.IO,
    read: (Response) -> T,
): T {
    val call = newCall(req)
    val response = suspendCancellableCoroutine { cont ->
        cont.invokeOnCancellation { call.cancel() }
        call.enqueue(object : Callback {
            override fun onFailure(call: Call, e: IOException) {
                if (cont.isActive) cont.resumeWithException(e)
            }

            override fun onResponse(call: Call, response: Response) {
                // Too late (cancelled meanwhile): close it instead of leaking it.
                cont.resume(response) { _, r, _ -> r.close() }
            }
        })
    }
    return try {
        withContext(io) { response.use(read) }
    } catch (e: CancellationException) {
        // Cancelled after the response arrived but before `read` ran: `use` never
        // closed it. Closing twice is harmless.
        response.close()
        throw e
    }
}
