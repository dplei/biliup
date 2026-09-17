'use client'
import { useRouter } from 'next/navigation'
import { useEffect } from 'react'

/** 旧路由占位：页面已改名「上传列表」并挪到 /uploads，这里只负责把旧书签带过去。 */
export default function MissingRedirect() {
  const router = useRouter()
  useEffect(() => {
    router.replace('/uploads')
  }, [router])
  return null
}
