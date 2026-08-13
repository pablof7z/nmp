package com.nmp.qualification.consumer;

import com.nmp.sdk.AcquisitionEvidence;
import com.nmp.sdk.NMPEngine;
import com.nmp.sdk.NMPError;
import com.nmp.sdk.RowBatch;
import com.nmp.sdk.SourceEvidence;
import com.nmp.sdk.SourceStatus;

/** Java-side proof that callers never need generated UniFFI types. */
public final class QualificationJava {
    private QualificationJava() {}

    public static boolean recognizesScopedError(RowBatch batch) {
        for (AcquisitionEvidence branch : batch.getEvidence()) {
            for (SourceEvidence source : branch.getSources()) {
                if (source.getStatus() == SourceStatus.Error.INSTANCE) {
                    return true;
                }
            }
        }
        return false;
    }

    public static boolean postCloseIsEngineClosed(NMPEngine engine) {
        try {
            engine.getSession().getCurrent();
            return false;
        } catch (Throwable error) {
            return error instanceof NMPError.EngineClosed;
        }
    }
}
