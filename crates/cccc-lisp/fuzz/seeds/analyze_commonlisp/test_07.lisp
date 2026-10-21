
            (defun safe (thunk)
              (handler-case (funcall thunk)
                (error (e)
                  (if (recoverable-p e) (retry) (abort)))))
        